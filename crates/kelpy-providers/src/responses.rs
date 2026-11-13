//! The OpenAI Responses API (`POST /responses`), streamed: OpenAI's own API.

use serde_json::{Value, json};

use crate::{ChatMessage, ChatRequest, Collected, ProviderError, Step, StopReason};

pub const OPENAI_BASE_URL: &str = "https://api.openai.com/v1";

/// The request for `chat`. `ask_reasoning` asks for the reasoning's encrypted content, as Codex CLI does with
/// ChatGPT's Codex backend; the API gives it without asking.
pub fn request(
    client: &reqwest::Client,
    base_url: &str,
    api_key: Option<&str>,
    model: &str,
    chat: &ChatRequest<'_>,
    ask_reasoning: bool,
) -> reqwest::RequestBuilder {
    // Kelpy keeps the conversation itself and sends it whole: nothing needs to stay stored at OpenAI.
    let mut body =
        json!({ "model": model, "input": input(chat.messages), "stream": true, "store": false });
    if ask_reasoning {
        body["include"] = json!(["reasoning.encrypted_content"]);
    }
    if !chat.system.is_empty() {
        body["instructions"] = json!(chat.system);
    }
    if !chat.tools.is_empty() {
        let tools: Vec<Value> = chat
            .tools
            .iter()
            .map(|tool| {
                json!({
                    "type": "function",
                    "name": tool.name,
                    "description": tool.description,
                    "parameters": tool.parameters,
                    "strict": false,
                })
            })
            .collect();
        body["tools"] = json!(tools);
        body["tool_choice"] = json!("auto");
        // One call at a time: each may wait for the administrator's approval.
        body["parallel_tool_calls"] = json!(false);
    }
    let builder = client
        .post(format!("{}/responses", base_url.trim_end_matches('/')))
        .json(&body);
    match api_key {
        Some(key) => builder.bearer_auth(key),
        None => builder,
    }
}

/// The conversation as typed items, which ChatGPT's Codex backend requires as well as the API accepts: messages,
/// function calls and their outputs.
fn input(messages: &[ChatMessage]) -> Vec<Value> {
    let mut items = Vec::with_capacity(messages.len());
    for message in messages {
        match message {
            ChatMessage::User { text, images } => {
                let (text, images) = ChatMessage::user_parts(text, images.as_ref());
                let mut content: Vec<Value> = images
                    .iter()
                    .map(|image| json!({ "type": "input_image", "image_url": image.data_url() }))
                    .collect();
                content.push(json!({ "type": "input_text", "text": text }));
                items.push(json!({ "type": "message", "role": "user", "content": content }));
            }
            ChatMessage::Assistant {
                text,
                calls,
                replay,
            } => {
                items.extend(replay.iter().cloned());
                if !text.is_empty() {
                    items.push(json!({
                        "type": "message",
                        "role": "assistant",
                        "content": [{ "type": "output_text", "text": text }],
                    }));
                }
                items.extend(calls.iter().map(|call| {
                    json!({
                        "type": "function_call",
                        "call_id": call.id,
                        "name": call.name,
                        "arguments": call.arguments,
                    })
                }));
            }
            ChatMessage::Tool(result) => items.push(json!({
                "type": "function_call_output",
                "call_id": result.call_id,
                "output": result.output,
            })),
        }
    }
    items
}

/// Reads one streamed event. Only `response.completed` ends an answer successfully; a stream that closes without
/// a final event is incomplete. A function call is taken whole from the item that ends it.
pub fn parse(data: &str, collected: &mut Collected) -> Result<Step, ProviderError> {
    let event: Value = serde_json::from_str(data)
        .map_err(|error| ProviderError::Protocol(format!("invalid event: {error}")))?;
    let delta = || event["delta"].as_str().map(str::to_owned);
    Ok(match event["type"].as_str() {
        Some("response.output_text.delta") => Step {
            text: delta(),
            ..Step::default()
        },
        // The model's own words declining the request: shown, and the answer counts as refused.
        Some("response.refusal.delta") => Step {
            text: delta(),
            stop: Some(StopReason::Refused),
            ..Step::default()
        },
        Some("response.output_item.done") if event["item"]["type"] == "function_call" => {
            let item = &event["item"];
            let (index, id) = (
                event["output_index"].as_u64().unwrap_or(0),
                item["call_id"].as_str(),
            );
            let name = item["name"].as_str().unwrap_or_default();
            collected.keep(name.len())?;
            name.clone_into(&mut collected.call(index, id)?.name);
            collected.arguments(index, id, item["arguments"].as_str().unwrap_or_default())?;
            Step::default()
        }
        // The model's reasoning before a call must go back with the call's result. Nothing is stored at OpenAI, so
        // only its encrypted content can, without the id that would name it there.
        Some("response.output_item.done")
            if event["item"]["type"] == "reasoning"
                && event["item"]["encrypted_content"].is_string() =>
        {
            let mut item = event["item"].clone();
            if let Some(fields) = item.as_object_mut() {
                fields.retain(|name, _| {
                    matches!(
                        name.as_str(),
                        "type" | "summary" | "content" | "encrypted_content"
                    )
                });
            }
            collected.replay(event["output_index"].as_u64().unwrap_or(0), Some(&item))?;
            Step::default()
        }
        Some("response.completed") => Step {
            input_tokens: event["response"]["usage"]["input_tokens"].as_u64(),
            ..Step::finished(None)
        },
        Some("response.incomplete") => {
            let reason = event["response"]["incomplete_details"]["reason"]
                .as_str()
                .unwrap_or("unknown");
            Step {
                input_tokens: event["response"]["usage"]["input_tokens"].as_u64(),
                ..Step::finished(Some(match reason {
                    "max_output_tokens" => StopReason::TooLong,
                    "content_filter" => StopReason::Refused,
                    other => StopReason::Incomplete(other.to_owned()),
                }))
            }
        }
        Some("response.failed") => return Err(api_error(&event["response"]["error"])),
        Some("error") => return Err(api_error(&event)),
        _ => Step::default(),
    })
}

/// An error object of the Responses API: a ChatGPT plan's refusal, its `message`, else its `code`.
fn api_error(error: &Value) -> ProviderError {
    let code = error["code"].as_str();
    let message = code
        .and_then(crate::plan_error)
        .or_else(|| error["message"].as_str())
        .or(code)
        .unwrap_or("the answer failed")
        .to_owned();
    ProviderError::Api {
        status: None,
        message,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ToolCall, ToolResult, ToolSpec};

    fn parse(data: &str) -> Result<Step, ProviderError> {
        super::parse(data, &mut Collected::default())
    }

    #[test]
    fn function_calls_are_taken_from_the_item_that_ends_them() {
        let mut collected = Collected::default();
        let added = r#"{"type":"response.output_item.added","output_index":1,"item":{"type":"function_call","call_id":"call_1","name":"disk_usage","arguments":""}}"#;
        let delta = r#"{"type":"response.function_call_arguments.delta","output_index":1,"delta":"{\"reason\""}"#;
        let done = r#"{"type":"response.output_item.done","output_index":1,"item":{"type":"function_call","call_id":"call_1","name":"disk_usage","arguments":"{\"reason\":\"space\"}"}}"#;
        for event in [added, delta, done] {
            assert_eq!(
                super::parse(event, &mut collected).unwrap(),
                Step::default()
            );
        }
        assert_eq!(
            collected.finish().0,
            [ToolCall {
                id: "call_1".into(),
                name: "disk_usage".into(),
                arguments: r#"{"reason":"space"}"#.into(),
            }]
        );
    }

    #[test]
    fn reasoning_goes_back_with_the_call_it_came_with() {
        let mut collected = Collected::default();
        let reasoning = r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"reasoning","id":"rs_1","status":"completed","summary":[],"encrypted_content":"gAAA"}}"#;
        // Without its encrypted content, a reasoning item cannot be sent back when nothing is stored at OpenAI.
        let unusable = r#"{"type":"response.output_item.done","output_index":1,"item":{"type":"reasoning","id":"rs_2","summary":[]}}"#;
        let call = r#"{"type":"response.output_item.done","output_index":2,"item":{"type":"function_call","call_id":"call_1","name":"disk_usage","arguments":"{}"}}"#;
        for event in [reasoning, unusable, call] {
            super::parse(event, &mut collected).unwrap();
        }
        let (calls, replay) = collected.finish();
        let item = json!({ "type": "reasoning", "summary": [], "encrypted_content": "gAAA" });
        assert_eq!(replay, std::slice::from_ref(&item));

        let messages = [ChatMessage::Assistant {
            text: String::new(),
            calls,
            replay,
        }];
        assert_eq!(input(&messages)[0], item);
        assert_eq!(input(&messages)[1]["type"], "function_call");
    }

    #[test]
    fn calls_and_outputs_go_back_as_items_with_the_tools() {
        let client = crate::Provider::new().unwrap().client;
        let messages = [
            ChatMessage::user("Disks?"),
            ChatMessage::Assistant {
                text: String::new(),
                calls: vec![ToolCall {
                    id: "call_1".into(),
                    name: "disk_usage".into(),
                    arguments: "{}".into(),
                }],
                replay: Vec::new(),
            },
            ChatMessage::Tool(ToolResult {
                call_id: "call_1".into(),
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
        let built = request(
            &client,
            OPENAI_BASE_URL,
            Some("key"),
            "gpt-test",
            &chat,
            false,
        )
        .build()
        .unwrap();
        let body: Value =
            serde_json::from_slice(built.body().unwrap().as_bytes().unwrap()).unwrap();
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["strict"], false);
        assert_eq!(body["parallel_tool_calls"], false);
        // The API gives the reasoning's encrypted content without being asked.
        assert!(body["include"].is_null());
        // No empty assistant message before the call.
        assert_eq!(body["input"][1]["type"], "function_call");
        assert_eq!(body["input"][1]["call_id"], "call_1");
        assert_eq!(body["input"][2]["type"], "function_call_output");
        assert_eq!(body["input"][2]["output"], "/ 40% used");
    }

    #[test]
    fn text_refusals_endings_and_errors_are_recognised() {
        let text = r#"{"type":"response.output_text.delta","item_id":"m","output_index":0,"content_index":0,"delta":"Hi"}"#;
        assert_eq!(parse(text).unwrap().text.as_deref(), Some("Hi"));
        let refusal = r#"{"type":"response.refusal.delta","delta":"I can't help with that."}"#;
        let step = parse(refusal).unwrap();
        assert_eq!(step.text.as_deref(), Some("I can't help with that."));
        assert_eq!(step.stop, Some(StopReason::Refused));
        assert!(!step.finished);

        let completed = r#"{"type":"response.completed","response":{"status":"completed"}}"#;
        assert_eq!(parse(completed).unwrap(), Step::finished(None));
        let too_long = r#"{"type":"response.incomplete","response":{"incomplete_details":{"reason":"max_output_tokens"}}}"#;
        assert_eq!(
            parse(too_long).unwrap(),
            Step::finished(Some(StopReason::TooLong))
        );
        let other = r#"{"type":"response.incomplete","response":{"incomplete_details":{"reason":"max_messages"}}}"#;
        assert_eq!(
            parse(other).unwrap(),
            Step::finished(Some(StopReason::Incomplete("max_messages".into())))
        );

        let failed = r#"{"type":"response.failed","response":{"status":"failed","error":{"code":"server_error","message":"The server had an error"}}}"#;
        assert!(
            matches!(parse(failed), Err(ProviderError::Api { message, .. }) if message == "The server had an error")
        );
        let limit = r#"{"type":"response.failed","response":{"error":{"code":"subscription_sharing_usage_limit_exceeded"}}}"#;
        assert!(
            matches!(parse(limit), Err(ProviderError::Api { message, .. }) if message.contains("chatgpt.com/settings/usage"))
        );
        let error = r#"{"type":"error","code":"invalid_model","message":"The model does not exist","param":"model"}"#;
        assert!(
            matches!(parse(error), Err(ProviderError::Api { message, .. }) if message == "The model does not exist")
        );
        let reasoning = r#"{"type":"response.reasoning_summary_text.delta","delta":"thinking"}"#;
        assert_eq!(parse(reasoning).unwrap(), Step::default());
    }

    #[test]
    fn requests_carry_instructions_history_and_no_storage() {
        let client = crate::Provider::new().unwrap().client;
        let messages = [ChatMessage::user("Hello"), ChatMessage::assistant("Hi!")];
        let chat = ChatRequest {
            system: "Be brief.",
            messages: &messages,
            tools: &[],
        };
        let built = request(
            &client,
            OPENAI_BASE_URL,
            Some("key"),
            "gpt-test",
            &chat,
            false,
        )
        .build()
        .unwrap();
        assert_eq!(built.url().as_str(), "https://api.openai.com/v1/responses");
        assert_eq!(built.headers()["authorization"], "Bearer key");
        let body: Value =
            serde_json::from_slice(built.body().unwrap().as_bytes().unwrap()).unwrap();
        assert_eq!(body["model"], "gpt-test");
        assert_eq!(body["instructions"], "Be brief.");
        assert_eq!(body["store"], false);
        assert_eq!(body["stream"], true);
        assert_eq!(body["input"][0]["content"][0]["type"], "input_text");
        assert_eq!(body["input"][1]["role"], "assistant");
        assert_eq!(body["input"][1]["content"][0]["type"], "output_text");
        assert_eq!(body["input"][1]["content"][0]["text"], "Hi!");
    }
}
