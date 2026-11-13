/*
 * SPDX-FileCopyrightText: 2026 bezenti
 *
 * SPDX-License-Identifier: AGPL-3.0-only
 */

//! TypeSafe (`POST /v1/systemone`, `Authorization: Bearer`): clasificación alojada, solo para el antispam.
//!
//! Según la documentación pública de la API de TypeSafe: `state` es el contenido a evaluar (aquí, el registro de
//! hechos), `questions` un mapa nombre → pregunta. Se pide una pregunta `choice` (`category`, con las categorías
//! como `criteria`: nombre → descripción) y una `noul` (sí/no) por señal (`signal_<nombre>`). La respuesta trae,
//! para `choice`, la opción elegida, su `confidence` y las `probabilities`; para `noul`, la probabilidad del sí.
//! `instructions` de `x:SpamAi` va como instrucciones de la pregunta de categoría.

use super::{
    AiModelRuntime, SpamAiConfig,
    clasificacion::AiVerdict,
    cliente::{AiError, AiResponse, json_str, json_u64, parse_json},
};
use serde_json::{Value, json};
use std::time::Instant;

pub fn build_request(config: &SpamAiConfig, model: &AiModelRuntime, facts: &Value) -> Value {
    let mut criteria = serde_json::Map::new();
    for category in &config.categories {
        let mut description = category.description.clone();
        if !category.includes.is_empty() {
            description.push_str(". Examples: ");
            description.push_str(&category.includes.join("; "));
        }
        criteria.insert(category.name.clone(), description.into());
    }
    let mut questions = serde_json::Map::new();
    questions.insert(
        "category".into(),
        json!({
            "type": "choice",
            "instructions": config.instructions.replace("\\n", "\n"),
            "criteria": Value::Object(criteria),
        }),
    );
    for signal in &config.signals {
        questions.insert(
            format!("signal_{}", signal.name),
            json!({"type": "noul", "instructions": signal.question}),
        );
    }
    json!({
        "model": model.model,
        "state": facts,
        "questions": Value::Object(questions),
    })
}

pub fn parse_response(
    config: &SpamAiConfig,
    model: &AiModelRuntime,
    body: &[u8],
) -> Result<(AiVerdict, AiResponse), AiError> {
    let value = parse_json(body)?;
    let answer = value
        .get("answers")
        .and_then(|a| a.get("category"))
        .ok_or_else(|| AiError::invalid("TypeSafe response without category answer"))?;
    let choice = answer
        .get("choice")
        .and_then(|c| c.as_str())
        .ok_or_else(|| AiError::invalid("TypeSafe response without choice"))?;
    let category = config
        .categories
        .iter()
        .position(|c| c.name == choice)
        .ok_or_else(|| AiError::invalid("TypeSafe returned an unknown category"))?;
    let probability = answer
        .get("probabilities")
        .and_then(|p| p.get(choice))
        .and_then(|p| p.as_f64())
        .or_else(|| answer.get("confidence").and_then(|c| c.as_f64()))
        .unwrap_or(0.0) as f32;
    let confidence = answer
        .get("confidence")
        .and_then(|c| c.as_f64())
        .map(|c| c as f32)
        .unwrap_or(probability);
    let signals = config
        .signals
        .iter()
        .map(|s| {
            value
                .get("answers")
                .and_then(|a| a.get(format!("signal_{}", s.name)))
                .and_then(|a| a.get("noul"))
                .and_then(|n| n.as_f64())
                .is_some_and(|n| n >= 0.5)
        })
        .collect();
    let reported = json_str(&value, &["model"]).unwrap_or_else(|| model.model.clone());
    let response = AiResponse {
        text: answer.to_string(),
        logprobs: Vec::new(),
        model: Some(reported.clone()),
        finish_reason: None,
        input_tokens: json_u64(&value, &["usage", "input_tokens"]),
        output_tokens: json_u64(&value, &["usage", "output_tokens"]),
        cached_tokens: 0,
        request_id: None,
    };
    Ok((
        AiVerdict {
            category,
            probability: probability.clamp(0.0, 1.0),
            confidence: confidence.clamp(0.0, 1.0),
            signals,
            explanation: None,
            model: reported,
        },
        response,
    ))
}

pub async fn classify(
    config: &SpamAiConfig,
    model: &AiModelRuntime,
    facts: &Value,
    deadline: Instant,
    session_id: u64,
) -> Result<AiVerdict, AiError> {
    let body = build_request(config, model, facts);
    let (raw, elapsed) = model.post_json(&body, deadline, session_id).await?;
    let (verdict, response) = parse_response(config, model, &raw.body)?;
    model.log_success(&response, elapsed, session_id);
    Ok(verdict)
}
