/*
 * SPDX-FileCopyrightText: 2026 bezenti
 *
 * SPDX-License-Identifier: AGPL-3.0-only
 */

//! Dialecto Google Gemini (`POST …/models/{modelo}:generateContent`, cabecera `x-goog-api-key`).
//!
//! - Salida estructurada: `generationConfig.responseMimeType = application/json` + `responseJsonSchema`.
//! - `logprobs`: `generationConfig.responseLogprobs` + `logprobs: 5`; la respuesta trae
//!   `logprobsResult.chosenCandidates` y `topCandidates`.
//! - `reasoning` (decisión bezenti): `Off` → `thinkingConfig.thinkingBudget: 0`; `Low`/`Medium`/`High` →
//!   `thinkingConfig.thinkingLevel` (`LOW`/`MEDIUM`/`HIGH`, modelos Gemini 3); `Default` no envía nada.
//! - Las partes con `thought: true` (resúmenes de razonamiento) no forman parte de la respuesta.

use super::{
    AiModelRuntime,
    cliente::{AiError, AiRequest, AiResponse, TokenLogprob, json_str, json_u64, parse_json},
};
use registry::schema::enums::AiReasoning;
use serde_json::{Value, json};

pub fn build_request(model: &AiModelRuntime, request: &AiRequest<'_>) -> Value {
    let mut generation = serde_json::Map::new();
    generation.insert("maxOutputTokens".into(), request.max_tokens.into());
    if let Some(temperature) = request.temperature {
        generation.insert("temperature".into(), temperature.into());
    }
    if let Some(schema) = request.json_schema {
        generation.insert("responseMimeType".into(), "application/json".into());
        generation.insert("responseJsonSchema".into(), schema.clone());
    }
    if request.logprobs {
        generation.insert("responseLogprobs".into(), true.into());
        generation.insert("logprobs".into(), 5.into());
    }
    match model.reasoning {
        AiReasoning::Default => {}
        AiReasoning::Off => {
            generation.insert("thinkingConfig".into(), json!({"thinkingBudget": 0}));
        }
        level => {
            let level = match level {
                AiReasoning::Low => "LOW",
                AiReasoning::Medium => "MEDIUM",
                _ => "HIGH",
            };
            generation.insert("thinkingConfig".into(), json!({"thinkingLevel": level}));
        }
    }

    let mut body = json!({
        "contents": [{"role": "user", "parts": [{"text": request.prompt}]}],
        "generationConfig": Value::Object(generation),
    });
    if let Some(system) = request.system {
        body.as_object_mut().unwrap().insert(
            "systemInstruction".into(),
            json!({"parts": [{"text": system}]}),
        );
    }
    body
}

pub fn parse_response(body: &[u8]) -> Result<AiResponse, AiError> {
    let value = parse_json(body)?;
    let Some(candidate) = value.get("candidates").and_then(|c| c.get(0)) else {
        return Err(AiError::invalid(
            json_str(&value, &["promptFeedback", "blockReason"])
                .map(|r| format!("Prompt blocked by the provider: {r}"))
                .unwrap_or_else(|| "AI response without candidates".into()),
        ));
    };
    let text = candidate
        .get("content")
        .and_then(|c| c.get("parts"))
        .and_then(|p| p.as_array())
        .map(|parts| {
            parts
                .iter()
                .filter(|p| !p.get("thought").and_then(|t| t.as_bool()).unwrap_or(false))
                .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default();
    let finish_reason = json_str(candidate, &["finishReason"]);
    if text.is_empty() {
        return Err(AiError::invalid(format!(
            "AI response without text (finish reason {})",
            finish_reason.as_deref().unwrap_or("unknown")
        )));
    }

    let mut logprobs = Vec::new();
    if let Some(result) = candidate.get("logprobsResult") {
        let chosen = result
            .get("chosenCandidates")
            .and_then(|c| c.as_array())
            .cloned()
            .unwrap_or_default();
        let top = result
            .get("topCandidates")
            .and_then(|c| c.as_array())
            .cloned()
            .unwrap_or_default();
        for (idx, item) in chosen.iter().enumerate() {
            let (Some(token), Some(logprob)) = (
                item.get("token").and_then(|t| t.as_str()),
                item.get("logProbability").and_then(|l| l.as_f64()),
            ) else {
                continue;
            };
            let alternatives = top
                .get(idx)
                .and_then(|t| t.get("candidates"))
                .and_then(|c| c.as_array())
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|c| {
                            Some((
                                c.get("token")?.as_str()?.to_string(),
                                c.get("logProbability")?.as_f64()?,
                            ))
                        })
                        .collect()
                })
                .unwrap_or_default();
            logprobs.push(TokenLogprob {
                token: token.to_string(),
                logprob,
                top: alternatives,
            });
        }
    }

    Ok(AiResponse {
        text,
        logprobs,
        model: json_str(&value, &["modelVersion"]),
        finish_reason,
        input_tokens: json_u64(&value, &["usageMetadata", "promptTokenCount"]),
        output_tokens: json_u64(&value, &["usageMetadata", "candidatesTokenCount"])
            + json_u64(&value, &["usageMetadata", "thoughtsTokenCount"]),
        cached_tokens: json_u64(&value, &["usageMetadata", "cachedContentTokenCount"]),
        request_id: json_str(&value, &["responseId"]),
    })
}

#[cfg(test)]
mod pruebas {
    use super::*;

    #[test]
    fn respuesta_y_bloqueo() {
        let body = br#"{"responseId":"r1","modelVersion":"gemini-x","candidates":[{"finishReason":"STOP",
            "content":{"role":"model","parts":[{"text":"pensando","thought":true},{"text":"Hola"}]},
            "logprobsResult":{"chosenCandidates":[{"token":"Hola","logProbability":-0.1}],
              "topCandidates":[{"candidates":[{"token":"Hola","logProbability":-0.1},{"token":"Chau","logProbability":-2.5}]}]}}],
            "usageMetadata":{"promptTokenCount":7,"candidatesTokenCount":1,"thoughtsTokenCount":4}}"#;
        let r = parse_response(body).unwrap();
        assert_eq!(r.text, "Hola");
        assert_eq!(r.output_tokens, 5);
        assert_eq!(r.logprobs[0].top.len(), 2);
        let e = parse_response(br#"{"promptFeedback":{"blockReason":"SAFETY"}}"#).unwrap_err();
        assert!(e.message.contains("SAFETY"));
    }
}
