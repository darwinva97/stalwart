/*
 * SPDX-FileCopyrightText: 2026 bezenti
 *
 * SPDX-License-Identifier: AGPL-3.0-only
 */

//! Dialecto OpenAI Chat Completions (`POST …/chat/completions`) y servidores compatibles.
//!
//! El perfil (`profile`) elige, según la documentación pública de Stalwart, el parámetro del límite de tokens,
//! el formato de la salida estructurada y la cabecera de autenticación:
//!
//! | perfil | límite de tokens | salida estructurada | autenticación |
//! |---|---|---|---|
//! | `OpenAi` | `max_completion_tokens` | `response_format` `json_schema` estricto | `Authorization: Bearer` |
//! | `Azure` | `max_completion_tokens` | `response_format` `json_schema` estricto | `api-key` |
//! | `Vllm` | `max_tokens` | `response_format` `json_schema` | `Authorization: Bearer` |
//! | `LlamaCpp` | `max_tokens` | `json_schema` (parámetro del servidor llama.cpp) | `Authorization: Bearer` |
//! | `Generic` | `max_tokens` | `response_format` `json_object` (el esquema va en las instrucciones) | `Authorization: Bearer` |
//!
//! `reasoning` (decisión bezenti): `Default` no envía nada; `Low`/`Medium`/`High` → `reasoning_effort`;
//! `Off` → `reasoning_effort: "none"` en OpenAI/Azure y `chat_template_kwargs.enable_thinking: false` en vLLM y
//! llama.cpp (plantillas tipo Qwen3); en `Generic` no se envía nada.

use super::{
    AiModelRuntime,
    cliente::{AiError, AiRequest, AiResponse, TokenLogprob, json_str, json_u64, parse_json},
};
use registry::schema::enums::{AiOpenAiProfile, AiReasoning};
use serde_json::{Value, json};

pub fn build_request(
    model: &AiModelRuntime,
    profile: AiOpenAiProfile,
    request: &AiRequest<'_>,
) -> Value {
    let mut messages = Vec::with_capacity(2);
    if let Some(system) = request.system {
        messages.push(json!({"role": "system", "content": system}));
    }
    messages.push(json!({"role": "user", "content": request.prompt}));

    let mut body = json!({
        "model": model.model,
        "messages": messages,
    });
    let obj = body.as_object_mut().unwrap();

    match profile {
        AiOpenAiProfile::OpenAi | AiOpenAiProfile::Azure => {
            obj.insert("max_completion_tokens".into(), request.max_tokens.into());
        }
        _ => {
            obj.insert("max_tokens".into(), request.max_tokens.into());
        }
    }
    if let Some(temperature) = request.temperature {
        obj.insert("temperature".into(), temperature.into());
    }

    match (model.reasoning, profile) {
        (AiReasoning::Default, _) => {}
        (AiReasoning::Off, AiOpenAiProfile::OpenAi | AiOpenAiProfile::Azure) => {
            obj.insert("reasoning_effort".into(), "none".into());
        }
        (AiReasoning::Off, AiOpenAiProfile::Vllm | AiOpenAiProfile::LlamaCpp) => {
            obj.insert(
                "chat_template_kwargs".into(),
                json!({"enable_thinking": false}),
            );
        }
        (AiReasoning::Off, AiOpenAiProfile::Generic) => {}
        (level, _) => {
            obj.insert("reasoning_effort".into(), effort(level).into());
        }
    }

    if let Some(schema) = request.json_schema {
        match profile {
            AiOpenAiProfile::OpenAi | AiOpenAiProfile::Azure | AiOpenAiProfile::Vllm => {
                obj.insert(
                    "response_format".into(),
                    json!({
                        "type": "json_schema",
                        "json_schema": {"name": "verdict", "strict": true, "schema": schema}
                    }),
                );
            }
            AiOpenAiProfile::LlamaCpp => {
                obj.insert("json_schema".into(), schema.clone());
            }
            AiOpenAiProfile::Generic => {
                obj.insert("response_format".into(), json!({"type": "json_object"}));
            }
        }
    }

    if request.logprobs {
        obj.insert("logprobs".into(), true.into());
        obj.insert("top_logprobs".into(), 5.into());
    }

    body
}

pub(crate) fn effort(level: AiReasoning) -> &'static str {
    match level {
        AiReasoning::Low => "low",
        AiReasoning::Medium => "medium",
        AiReasoning::High => "high",
        AiReasoning::Off => "none",
        AiReasoning::Default => "medium",
    }
}

pub fn parse_response(body: &[u8]) -> Result<AiResponse, AiError> {
    let value = parse_json(body)?;
    let choice = value
        .get("choices")
        .and_then(|c| c.get(0))
        .ok_or_else(|| AiError::invalid("AI response without choices"))?;
    let message = choice
        .get("message")
        .ok_or_else(|| AiError::invalid("AI response without message"))?;
    let text = match message.get("content") {
        Some(Value::String(text)) => text.clone(),
        // Algunos servidores devuelven el contenido como lista de partes.
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join(""),
        _ => {
            return Err(AiError::invalid(
                json_str(message, &["refusal"])
                    .map(|r| format!("Refused by the model: {r}"))
                    .unwrap_or_else(|| "AI response without text".into()),
            ));
        }
    };

    let logprobs = choice
        .get("logprobs")
        .and_then(|l| l.get("content"))
        .and_then(|c| c.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    Some(TokenLogprob {
                        token: item.get("token")?.as_str()?.to_string(),
                        logprob: item.get("logprob")?.as_f64()?,
                        top: item
                            .get("top_logprobs")
                            .and_then(|t| t.as_array())
                            .map(|top| {
                                top.iter()
                                    .filter_map(|t| {
                                        Some((
                                            t.get("token")?.as_str()?.to_string(),
                                            t.get("logprob")?.as_f64()?,
                                        ))
                                    })
                                    .collect()
                            })
                            .unwrap_or_default(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    Ok(AiResponse {
        text,
        logprobs,
        model: json_str(&value, &["model"]),
        finish_reason: json_str(choice, &["finish_reason"]),
        input_tokens: json_u64(&value, &["usage", "prompt_tokens"]),
        output_tokens: json_u64(&value, &["usage", "completion_tokens"]),
        cached_tokens: json_u64(&value, &["usage", "prompt_tokens_details", "cached_tokens"]),
        request_id: json_str(&value, &["id"]),
    })
}

#[cfg(test)]
mod pruebas {
    use super::*;

    #[test]
    fn respuesta_con_logprobs() {
        let body = br#"{"id":"chatcmpl-1","model":"gpt-x","choices":[{"index":0,"finish_reason":"stop",
            "message":{"role":"assistant","content":"{\"category\":\"phishing\"}"},
            "logprobs":{"content":[{"token":"{\"","logprob":0.0,"top_logprobs":[]},
            {"token":"phishing","logprob":-0.05,"top_logprobs":[{"token":"phishing","logprob":-0.05},{"token":"legit","logprob":-3.2}]}]}}],
            "usage":{"prompt_tokens":120,"completion_tokens":9,"prompt_tokens_details":{"cached_tokens":64}}}"#;
        let r = parse_response(body).unwrap();
        assert_eq!(r.text, "{\"category\":\"phishing\"}");
        assert_eq!(r.input_tokens, 120);
        assert_eq!(r.output_tokens, 9);
        assert_eq!(r.cached_tokens, 64);
        assert_eq!(r.finish_reason.as_deref(), Some("stop"));
        assert_eq!(r.request_id.as_deref(), Some("chatcmpl-1"));
        assert_eq!(r.logprobs.len(), 2);
        assert_eq!(r.logprobs[1].top.len(), 2);
    }

    #[test]
    fn respuestas_invalidas() {
        assert!(parse_response(b"no json").is_err());
        assert!(parse_response(br#"{"choices":[]}"#).is_err());
        let e = parse_response(br#"{"choices":[{"message":{"content":null,"refusal":"no"}}]}"#)
            .unwrap_err();
        assert!(e.message.contains("Refused"));
    }
}
