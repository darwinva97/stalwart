/*
 * SPDX-FileCopyrightText: 2026 bezenti
 *
 * SPDX-License-Identifier: AGPL-3.0-only
 */

//! Dialecto Ollama (`POST /api/chat`, sin streaming).
//!
//! - Salida estructurada: `format` con el esquema JSON.
//! - `options.num_predict` (límite de tokens) y `options.temperature`.
//! - `keep_alive` (`keepAlive` del modelo) en segundos.
//! - `reasoning` (decisión bezenti): `Off` → `think: false`; `Low`/`Medium`/`High` → `think: true`;
//!   `Default` no envía nada.
//! - `logprobs`: `logprobs: true` + `top_logprobs: 5` (versiones de Ollama que lo admiten).

use super::{
    AiModelRuntime,
    cliente::{AiError, AiRequest, AiResponse, TokenLogprob, json_str, json_u64, parse_json},
};
use registry::schema::enums::AiReasoning;
use serde_json::{Value, json};

pub fn build_request(model: &AiModelRuntime, request: &AiRequest<'_>) -> Value {
    let mut messages = Vec::with_capacity(2);
    if let Some(system) = request.system {
        messages.push(json!({"role": "system", "content": system}));
    }
    messages.push(json!({"role": "user", "content": request.prompt}));
    let mut options = serde_json::Map::new();
    options.insert("num_predict".into(), request.max_tokens.into());
    if let Some(temperature) = request.temperature {
        options.insert("temperature".into(), temperature.into());
    }
    let mut body = json!({
        "model": model.model,
        "messages": messages,
        "stream": false,
        "options": Value::Object(options),
    });
    let obj = body.as_object_mut().unwrap();
    if let Some(schema) = request.json_schema {
        obj.insert("format".into(), schema.clone());
    }
    if let Some(keep_alive) = model.keep_alive {
        obj.insert(
            "keep_alive".into(),
            format!("{}s", keep_alive.as_secs()).into(),
        );
    }
    match model.reasoning {
        AiReasoning::Default => {}
        AiReasoning::Off => {
            obj.insert("think".into(), false.into());
        }
        _ => {
            obj.insert("think".into(), true.into());
        }
    }
    if request.logprobs {
        obj.insert("logprobs".into(), true.into());
        obj.insert("top_logprobs".into(), 5.into());
    }
    body
}

pub fn parse_response(body: &[u8]) -> Result<AiResponse, AiError> {
    let value = parse_json(body)?;
    if let Some(error) = json_str(&value, &["error"]) {
        return Err(AiError::invalid(format!("Ollama error: {error}")));
    }
    let text = json_str(&value, &["message", "content"])
        .ok_or_else(|| AiError::invalid("AI response without text"))?;
    let logprobs = value
        .get("logprobs")
        .and_then(|l| l.as_array())
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
        finish_reason: json_str(&value, &["done_reason"]),
        input_tokens: json_u64(&value, &["prompt_eval_count"]),
        output_tokens: json_u64(&value, &["eval_count"]),
        cached_tokens: 0,
        request_id: None,
    })
}

#[cfg(test)]
mod pruebas {
    use super::*;

    #[test]
    fn respuesta_ollama() {
        let body = br#"{"model":"qwen3:8b","message":{"role":"assistant","content":"ok","thinking":"..."},
            "done":true,"done_reason":"stop","prompt_eval_count":30,"eval_count":2,
            "logprobs":[{"token":"ok","logprob":-0.01,"top_logprobs":[{"token":"ok","logprob":-0.01}]}]}"#;
        let r = parse_response(body).unwrap();
        assert_eq!(r.text, "ok");
        assert_eq!(r.input_tokens, 30);
        assert_eq!(r.logprobs.len(), 1);
        assert!(parse_response(br#"{"error":"model 'x' not found"}"#).is_err());
    }
}
