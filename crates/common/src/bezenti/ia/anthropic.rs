/*
 * SPDX-FileCopyrightText: 2026 bezenti
 *
 * SPDX-License-Identifier: AGPL-3.0-only
 */

//! Dialecto Anthropic Messages (`POST /v1/messages`, cabeceras `x-api-key` y `anthropic-version`).
//!
//! - Salida estructurada con `output_config.format` (`json_schema`), según la documentación pública de la API.
//! - `reasoning` (decisión bezenti): `Low`/`Medium`/`High` → `output_config.effort`; `Default` y `Off` no
//!   envían nada (los modelos actuales no admiten desactivar el razonamiento y deciden solos; para modelos
//!   antiguos sin razonamiento por defecto, `Off` ya es lo que hacen).
//! - `temperature` solo se envía si está configurada (los modelos más recientes la rechazan).
//! - Sin `logprobs`: la confianza es la que declara el modelo (`verbalHigh/Medium/Low`).

use super::{
    AiModelRuntime,
    cliente::{AiError, AiRequest, AiResponse, json_str, json_u64, parse_json},
};
use registry::schema::enums::AiReasoning;
use serde_json::{Value, json};

pub const API_VERSION: &str = "2023-06-01";

pub fn build_request(model: &AiModelRuntime, request: &AiRequest<'_>) -> Value {
    let mut body = json!({
        "model": model.model,
        "max_tokens": request.max_tokens,
        "messages": [{"role": "user", "content": request.prompt}],
    });
    let obj = body.as_object_mut().unwrap();
    if let Some(system) = request.system {
        obj.insert("system".into(), system.into());
    }
    if let Some(temperature) = request.temperature {
        obj.insert("temperature".into(), temperature.into());
    }
    let mut output_config = serde_json::Map::new();
    if matches!(
        model.reasoning,
        AiReasoning::Low | AiReasoning::Medium | AiReasoning::High
    ) {
        output_config.insert(
            "effort".into(),
            super::openai::effort(model.reasoning).into(),
        );
    }
    if let Some(schema) = request.json_schema {
        output_config.insert(
            "format".into(),
            json!({"type": "json_schema", "schema": schema}),
        );
    }
    if !output_config.is_empty() {
        obj.insert("output_config".into(), Value::Object(output_config));
    }
    body
}

pub fn parse_response(body: &[u8]) -> Result<AiResponse, AiError> {
    let value = parse_json(body)?;
    let stop_reason = json_str(&value, &["stop_reason"]);
    if stop_reason.as_deref() == Some("refusal") {
        return Err(AiError::invalid("Refused by the model"));
    }
    let content = value
        .get("content")
        .and_then(|c| c.as_array())
        .ok_or_else(|| AiError::invalid("AI response without content"))?;
    let text = content
        .iter()
        .filter(|block| block.get("type").and_then(|t| t.as_str()) == Some("text"))
        .filter_map(|block| block.get("text").and_then(|t| t.as_str()))
        .collect::<Vec<_>>()
        .join("");
    if text.is_empty() {
        return Err(AiError::invalid("AI response without text"));
    }
    Ok(AiResponse {
        text,
        logprobs: Vec::new(),
        model: json_str(&value, &["model"]),
        finish_reason: stop_reason,
        input_tokens: json_u64(&value, &["usage", "input_tokens"]),
        output_tokens: json_u64(&value, &["usage", "output_tokens"]),
        cached_tokens: json_u64(&value, &["usage", "cache_read_input_tokens"]),
        request_id: json_str(&value, &["id"]),
    })
}

#[cfg(test)]
mod pruebas {
    use super::*;

    #[test]
    fn respuesta_con_razonamiento() {
        let body = br#"{"id":"msg_1","type":"message","model":"claude-x","stop_reason":"end_turn",
            "content":[{"type":"thinking","thinking":""},{"type":"text","text":"Hola"}],
            "usage":{"input_tokens":10,"output_tokens":3,"cache_read_input_tokens":2}}"#;
        let r = parse_response(body).unwrap();
        assert_eq!(r.text, "Hola");
        assert_eq!(r.cached_tokens, 2);
        assert!(parse_response(br#"{"stop_reason":"refusal","content":[]}"#).is_err());
        assert!(parse_response(br#"{"content":[]}"#).is_err());
    }
}
