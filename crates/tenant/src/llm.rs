/*
 * SPDX-FileCopyrightText: 2026 Stalwart contributors
 *
 * SPDX-License-Identifier: AGPL-3.0-only
 *
 * AI/LLM client core: request-body construction and response parsing for
 * OpenAI-compatible chat and text completion endpoints. Transport (HTTP) is
 * intentionally left to the caller so this logic stays pure and testable.
 */

use serde::{Deserialize, Serialize};

/// The completion protocol an endpoint speaks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApiType {
    ChatCompletion,
    TextCompletion,
}

/// Configuration for an OpenAI-compatible completion endpoint.
#[derive(Clone, Debug)]
pub struct AiApiConfig {
    id: String,
    api_type: ApiType,
    model: String,
    default_temperature: f64,
}

#[derive(Serialize)]
struct ChatCompletionRequest<'a> {
    model: &'a str,
    messages: Vec<Message<'a>>,
    temperature: f64,
}

#[derive(Serialize)]
struct Message<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Serialize)]
struct TextCompletionRequest<'a> {
    model: &'a str,
    prompt: &'a str,
    temperature: f64,
}

impl AiApiConfig {
    pub fn new(
        id: impl Into<String>,
        api_type: ApiType,
        model: impl Into<String>,
        default_temperature: f64,
    ) -> Self {
        AiApiConfig {
            id: id.into(),
            api_type,
            model: model.into(),
            default_temperature,
        }
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    /// Builds the JSON request body for `prompt`, using `temperature` when
    /// provided or the configured default otherwise.
    pub fn build_request_body(&self, prompt: &str, temperature: Option<f64>) -> String {
        let temperature = temperature.unwrap_or(self.default_temperature);
        match self.api_type {
            ApiType::ChatCompletion => serde_json::to_string(&ChatCompletionRequest {
                model: &self.model,
                messages: vec![Message {
                    role: "user",
                    content: prompt,
                }],
                temperature,
            }),
            ApiType::TextCompletion => serde_json::to_string(&TextCompletionRequest {
                model: &self.model,
                prompt,
                temperature,
            }),
        }
        .expect("request bodies are always serializable")
    }

    /// Extracts the first non-empty completion from a raw response body.
    pub fn parse_response(&self, body: &[u8]) -> Result<String, LlmError> {
        let text = match self.api_type {
            ApiType::ChatCompletion => serde_json::from_slice::<ChatCompletionResponse>(body)
                .map_err(|_| LlmError::Decode)?
                .choices
                .into_iter()
                .next()
                .map(|c| c.message.content),
            ApiType::TextCompletion => serde_json::from_slice::<TextCompletionResponse>(body)
                .map_err(|_| LlmError::Decode)?
                .choices
                .into_iter()
                .next()
                .map(|c| c.text),
        };

        text.filter(|t| !t.is_empty()).ok_or(LlmError::EmptyResponse)
    }
}

/// Failure modes when parsing a completion response.
#[derive(Debug, PartialEq, Eq)]
pub enum LlmError {
    /// The body was not valid JSON for the configured protocol.
    Decode,
    /// The endpoint returned no usable completion.
    EmptyResponse,
}

#[derive(Deserialize)]
struct ChatCompletionResponse {
    choices: Vec<ChatCompletionChoice>,
}

#[derive(Deserialize)]
struct ChatCompletionChoice {
    message: ResponseMessage,
}

#[derive(Deserialize)]
struct ResponseMessage {
    content: String,
}

#[derive(Deserialize)]
struct TextCompletionResponse {
    choices: Vec<TextCompletionChoice>,
}

#[derive(Deserialize)]
struct TextCompletionChoice {
    text: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn chat_config() -> AiApiConfig {
        AiApiConfig::new("gpt", ApiType::ChatCompletion, "gpt-4o", 0.7)
    }

    #[test]
    fn given_a_chat_config_when_building_a_body_then_it_carries_prompt_and_default_temperature() {
        let body = chat_config().build_request_body("hello", None);
        let json: Value = serde_json::from_str(&body).unwrap();

        assert_eq!(json["model"], "gpt-4o");
        assert_eq!(json["temperature"], 0.7);
        assert_eq!(json["messages"][0]["role"], "user");
        assert_eq!(json["messages"][0]["content"], "hello");
    }

    #[test]
    fn given_an_explicit_temperature_when_building_a_body_then_it_overrides_the_default() {
        let body = chat_config().build_request_body("hi", Some(0.1));
        let json: Value = serde_json::from_str(&body).unwrap();

        assert_eq!(json["temperature"], 0.1);
    }

    #[test]
    fn given_a_text_config_when_building_a_body_then_it_carries_a_prompt_field() {
        let config = AiApiConfig::new("comp", ApiType::TextCompletion, "davinci", 0.5);

        let body = config.build_request_body("once upon a time", None);
        let json: Value = serde_json::from_str(&body).unwrap();

        assert_eq!(json["prompt"], "once upon a time");
        assert!(json.get("messages").is_none());
    }

    #[test]
    fn given_a_chat_response_when_parsed_then_the_first_choice_content_is_returned() {
        let raw = br#"{"choices":[{"message":{"role":"assistant","content":"hi there"}}]}"#;

        let out = chat_config().parse_response(raw).unwrap();

        assert_eq!(out, "hi there");
    }

    #[test]
    fn given_a_text_response_when_parsed_then_the_first_choice_text_is_returned() {
        let config = AiApiConfig::new("comp", ApiType::TextCompletion, "davinci", 0.5);
        let raw = br#"{"choices":[{"text":"a story"}]}"#;

        let out = config.parse_response(raw).unwrap();

        assert_eq!(out, "a story");
    }

    #[test]
    fn given_a_response_with_no_choices_when_parsed_then_it_errors() {
        let raw = br#"{"choices":[]}"#;

        assert_eq!(
            chat_config().parse_response(raw),
            Err(LlmError::EmptyResponse)
        );
    }

    #[test]
    fn given_a_response_whose_choice_is_empty_when_parsed_then_it_errors() {
        let raw = br#"{"choices":[{"message":{"role":"assistant","content":""}}]}"#;

        assert_eq!(
            chat_config().parse_response(raw),
            Err(LlmError::EmptyResponse)
        );
    }

    #[test]
    fn given_malformed_json_when_parsed_then_it_reports_a_decode_error() {
        assert_eq!(
            chat_config().parse_response(b"not json"),
            Err(LlmError::Decode)
        );
    }
}
