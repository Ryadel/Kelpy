//! The Anthropic Messages API (`POST /v1/messages`), streamed.

use serde_json::{Value, json};

use crate::{ChatMessage, ChatRequest, Collected, ProviderError, Step, StopReason, ToolCall};

pub const DEFAULT_BASE_URL: &str = "https://api.anthropic.com/v1";
pub const DEFAULT_MODEL: &str = "claude-opus-5-5";
const API_VERSION: &str = "2026-10-21";
/// Streaming has no request timeout to fear, so the model gets room for long answers.
const MAX_TOKENS: u32 = 64_000;
/// Server-side fallback: when a model's safety classifiers decline a request, Anthropic retries it on the model
/// it recommends for that case, on the same stream.
const FALLBACK_BETA: &str = "server-side-fallback-2026-07-01";
const FALLBACK_MODELS: &[&str] = &[
    "claude-fable-5-1",
    "claude-opus-5-5",
    "claude-opus-5",
    "claude-sonnet-5-5",
];

pub fn request(
    client: &reqwest::Client,
    base_url: &str,
    api_key: Option<&str>,
    model: &str,
    chat: &ChatRequest<'_>,
) -> reqwest::RequestBuilder {
    let mut body = json!({
        "model": model,
        "max_tokens": MAX_TOKENS,
        "messages": messages(chat.messages),
        "stream": true,
    });
    if !chat.system.is_empty() {
        body["system"] = json!(chat.system);
    }
    if !chat.tools.is_empty() {
        let tools: Vec<Value> = chat
            .tools
            .iter()
            .map(|tool| {
                json!({
                    "name": tool.name,
                    "description": tool.description,
                    "input_schema": tool.parameters,
                })
            })
            .collect();
        body["tools"] = json!(tools);
    }
    let mut builder = authorize(
        client.post(format!("{}/messages", base_url.trim_end_matches('/'))),
        api_key,
    );
    // Fallbacks exist on Anthropic's own API only, not on proxies or other platforms behind a custom base URL.
    if base_url == DEFAULT_BASE_URL && FALLBACK_MODELS.contains(&model) {
        body["fallbacks"] = json!("default");
        builder = builder.header("anthropic-beta", FALLBACK_BETA);
    }
    builder.json(&body)
}

/// The messages as content blocks. Tool results go in a user message; consecutive messages of one role are joined,
/// as the roles must alternate.
fn messages(messages: &[ChatMessage]) -> Vec<Value> {
    let mut joined: Vec<Value> = Vec::with_capacity(messages.len());
    for message in messages {
        let (role, blocks) = match message {
            ChatMessage::User { text, images } => {
                let (text, images) = ChatMessage::user_parts(text, images.as_ref());
                // Images first, as Anthropic advises.
                let mut blocks: Vec<Value> = images
                    .iter()
                    .map(|image| {
                        json!({
                            "type": "image",
                            "source": { "type": "base64", "media_type": image.media_type, "data": image.data },
                        })
                    })
                    .collect();
                blocks.push(json!({ "type": "text", "text": text }));
                ("user", blocks)
            }
            ChatMessage::Assistant {
                text,
                calls,
                replay,
            } => {
                let mut blocks = replay.clone();
                if !text.is_empty() {
                    blocks.push(json!({ "type": "text", "text": text }));
                }
                blocks.extend(calls.iter().map(|call| {
                    json!({ "type": "tool_use", "id": call.id, "name": call.name, "input": input(call) })
                }));
                ("assistant", blocks)
            }
            ChatMessage::Tool(result) => (
                "user",
                vec![json!({
                    "type": "tool_result",
                    "tool_use_id": result.call_id,
                    "content": result.output,
                    "is_error": result.is_error,
                })],
            ),
        };
        if blocks.is_empty() {
            continue;
        }
        match joined.last_mut() {
            Some(last) if last["role"] == role => {
                if let Some(content) = last["content"].as_array_mut() {
                    content.extend(blocks);
                }
            }
            _ => joined.push(json!({ "role": role, "content": blocks })),
        }
    }
    joined
}

/// A call's arguments as the object Anthropic expects: arguments that are not an object, which only a broken
/// stream leaves, become an empty one.
fn input(call: &ToolCall) -> Value {
    serde_json::from_str::<Value>(&call.arguments)
        .ok()
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}))
}

/// Adds the API version and the key, as every Anthropic request carries them.
pub fn authorize(
    builder: reqwest::RequestBuilder,
    api_key: Option<&str>,
) -> reqwest::RequestBuilder {
    let builder = builder.header("anthropic-version", API_VERSION);
    match api_key {
        Some(key) => builder.header("x-api-key", key),
        None => builder,
    }
}

/// Reads one event. Only text reaches the user; tool calls and thinking blocks are collected, the thinking to send
/// back with the calls, as Anthropic requires. After a fallback the stream simply continues with the other model.
pub fn parse(data: &str, collected: &mut Collected) -> Result<Step, ProviderError> {
    let event: Value = serde_json::from_str(data)
        .map_err(|error| ProviderError::Protocol(format!("invalid event: {error}")))?;
    let index = event["index"].as_u64().unwrap_or(0);
    let delta = &event["delta"];
    let piece = |field: &str| delta[field].as_str().unwrap_or_default();
    Ok(match event["type"].as_str() {
        Some("content_block_start") => {
            let block = &event["content_block"];
            match block["type"].as_str() {
                Some("tool_use") => {
                    let name = block["name"].as_str().unwrap_or_default();
                    collected.keep(name.len())?;
                    name.clone_into(&mut collected.call(index, block["id"].as_str())?.name);
                }
                Some("thinking" | "redacted_thinking") => {
                    collected.replay(index, Some(block))?;
                }
                _ => {}
            }
            Step::default()
        }
        Some("content_block_delta") => match delta["type"].as_str() {
            Some("text_delta") => Step {
                text: delta["text"].as_str().map(str::to_owned),
                ..Step::default()
            },
            Some("input_json_delta") => {
                collected.arguments(index, None, piece("partial_json"))?;
                Step::default()
            }
            Some(kind @ ("thinking_delta" | "signature_delta")) => {
                let field = if kind == "thinking_delta" {
                    "thinking"
                } else {
                    "signature"
                };
                collected.keep(piece(field).len())?;
                if let Some(block) = collected.replay(index, None)? {
                    match &mut block[field] {
                        Value::String(text) => text.push_str(piece(field)),
                        other => *other = json!(piece(field)),
                    }
                }
                Step::default()
            }
            _ => Step::default(),
        },
        Some("message_start") => Step {
            input_tokens: input_tokens(&event["message"]["usage"]),
            ..Step::default()
        },
        // Its usage, when it has the input, is the final count.
        Some("message_delta") => Step {
            stop: delta["stop_reason"]
                .as_str()
                .map(StopReason::from_anthropic),
            input_tokens: input_tokens(&event["usage"]),
            ..Step::default()
        },
        Some("message_stop") => Step::finished(None),
        Some("error") => {
            let message = event["error"]["message"]
                .as_str()
                .unwrap_or("unknown error")
                .to_owned();
            return Err(ProviderError::Api {
                status: None,
                message,
            });
        }
        _ => Step::default(),
    })
}

/// The whole input of a request in a usage object: the tokens read from the prompt cache and written to it are
/// counted apart from the others.
fn input_tokens(usage: &Value) -> Option<u64> {
    let input = usage["input_tokens"].as_u64()?;
    let cached = ["cache_creation_input_tokens", "cache_read_input_tokens"]
        .iter()
        .filter_map(|field| usage[field].as_u64())
        .sum::<u64>();
    Some(input + cached)
}

impl StopReason {
    fn from_anthropic(reason: &str) -> Self {
        match reason {
            "end_turn" | "stop_sequence" | "tool_use" => Self::Complete,
            "max_tokens" | "model_context_window_exceeded" => Self::TooLong,
            "refusal" => Self::Refused,
            other => Self::Other(other.to_owned()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ToolResult, ToolSpec};

    fn parsed(data: &str) -> Step {
        parse(data, &mut Collected::default()).unwrap()
    }

    #[test]
    fn text_deltas_stop_reasons_and_errors_are_recognised() {
        let text =
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hi"}}"#;
        assert_eq!(parsed(text).text.as_deref(), Some("Hi"));
        let stop = r#"{"type":"message_delta","delta":{"stop_reason":"refusal"},"usage":{}}"#;
        assert_eq!(parsed(stop).stop, Some(StopReason::Refused));
        assert_eq!(parsed(stop).input_tokens, None);
        let start = r#"{"type":"message_start","message":{"usage":{"input_tokens":5,"cache_creation_input_tokens":7,"output_tokens":1}}}"#;
        assert_eq!(parsed(start).input_tokens, Some(12));
        assert_eq!(parsed(r#"{"type":"message_stop"}"#), Step::finished(None));
        let error =
            r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#;
        assert!(
            matches!(parse(error, &mut Collected::default()), Err(ProviderError::Api { message, .. }) if message == "Overloaded")
        );
    }

    #[test]
    fn tool_calls_and_thinking_are_collected_from_their_blocks() {
        let mut collected = Collected::default();
        for event in [
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"Check "}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"the disks."}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig"}}"#,
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_1","name":"disk_usage","input":{}}}"#,
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"reason\":"}}"#,
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"\"space\"}"}}"#,
        ] {
            // Thinking never reaches the user.
            assert_eq!(parse(event, &mut collected).unwrap().text, None);
        }
        let stop = r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"}}"#;
        assert_eq!(
            parse(stop, &mut collected).unwrap().stop,
            Some(StopReason::Complete)
        );
        let (calls, replay) = collected.finish();
        assert_eq!(
            calls,
            [ToolCall {
                id: "toolu_1".into(),
                name: "disk_usage".into(),
                arguments: r#"{"reason":"space"}"#.into(),
            }]
        );
        assert_eq!(
            replay,
            [json!({ "type": "thinking", "thinking": "Check the disks.", "signature": "sig" })]
        );
    }

    #[test]
    fn requests_carry_version_system_and_fallbacks() {
        let client = crate::Provider::new().unwrap().client;
        let messages = [ChatMessage::user("Hello")];
        let chat = ChatRequest {
            system: "Be brief.",
            messages: &messages,
            tools: &[],
        };
        let built = request(&client, DEFAULT_BASE_URL, Some("key"), DEFAULT_MODEL, &chat)
            .build()
            .unwrap();
        assert_eq!(
            built.url().as_str(),
            "https://api.anthropic.com/v1/messages"
        );
        assert_eq!(built.headers()["anthropic-version"], API_VERSION);
        assert_eq!(built.headers()["anthropic-beta"], FALLBACK_BETA);
        let body: Value =
            serde_json::from_slice(built.body().unwrap().as_bytes().unwrap()).unwrap();
        assert_eq!(body["system"], "Be brief.");
        assert_eq!(body["fallbacks"], "default");
        assert_eq!(body["messages"][0]["role"], "user");
        assert!(body.get("temperature").is_none());
        assert!(body.get("tools").is_none());

        let proxied = request(
            &client,
            "https://proxy.example/v1",
            Some("key"),
            DEFAULT_MODEL,
            &chat,
        )
        .build()
        .unwrap();
        assert!(!proxied.headers().contains_key("anthropic-beta"));
    }

    #[test]
    fn calls_and_results_go_back_as_blocks_of_alternating_roles() {
        let client = crate::Provider::new().unwrap().client;
        let thinking = json!({ "type": "thinking", "thinking": "Check.", "signature": "sig" });
        let messages = [
            ChatMessage::user("Disks?"),
            ChatMessage::Assistant {
                text: "Looking.".into(),
                calls: vec![ToolCall {
                    id: "toolu_1".into(),
                    name: "disk_usage".into(),
                    arguments: r#"{"reason":"space"}"#.into(),
                }],
                replay: vec![thinking.clone()],
            },
            ChatMessage::Tool(ToolResult {
                call_id: "toolu_1".into(),
                output: "/ 40% used".into(),
                is_error: false,
            }),
        ];
        let tools = [ToolSpec {
            name: "disk_usage".into(),
            description: "Shows the disks.".into(),
            parameters: json!({ "type": "object" }),
        }];
        let chat = ChatRequest {
            system: "",
            messages: &messages,
            tools: &tools,
        };
        let built = request(&client, DEFAULT_BASE_URL, Some("key"), DEFAULT_MODEL, &chat)
            .build()
            .unwrap();
        let body: Value =
            serde_json::from_slice(built.body().unwrap().as_bytes().unwrap()).unwrap();
        assert_eq!(body["tools"][0]["name"], "disk_usage");
        assert_eq!(body["tools"][0]["input_schema"]["type"], "object");
        let assistant = &body["messages"][1];
        assert_eq!(assistant["role"], "assistant");
        assert_eq!(assistant["content"][0], thinking);
        assert_eq!(assistant["content"][1]["text"], "Looking.");
        assert_eq!(assistant["content"][2]["type"], "tool_use");
        assert_eq!(assistant["content"][2]["input"]["reason"], "space");
        let results = &body["messages"][2];
        assert_eq!(results["role"], "user");
        assert_eq!(results["content"][0]["type"], "tool_result");
        assert_eq!(results["content"][0]["tool_use_id"], "toolu_1");
        assert_eq!(results["content"][0]["is_error"], false);
    }
}
