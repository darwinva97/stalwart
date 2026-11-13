/*
 * SPDX-FileCopyrightText: 2026 bezenti
 *
 * SPDX-License-Identifier: AGPL-3.0-only
 */

//! Clasificación antispam con un modelo: instrucciones, esquema del veredicto y lectura de la respuesta.
//!
//! El modelo recibe como instrucciones (system prompt) el texto `instructions` de `x:SpamAi` seguido de la lista
//! de categorías (nombre, descripción, ejemplos) y de señales (pregunta sí/no), y como mensaje del usuario el
//! registro de hechos en JSON (`{"email": {…}}`). Responde con un objeto JSON:
//!
//! ```json
//! {"category": "phishing", "confidence": "high", "signals": {"impersonation": true}, "explanation": "…"}
//! ```
//!
//! Probabilidad y confianza (decisión bezenti, ver ia.md §5.4):
//! - con `logprobs`, la probabilidad `p` es `exp(Σ logprob)` de los tokens que forman el nombre de la categoría
//!   y la confianza es el margen entre el token elegido y la mejor alternativa en el primer token del nombre;
//! - sin `logprobs`, `p` sale de la confianza declarada (`verbalHigh/Medium/Low`) y la confianza es `2p − 1`
//!   (margen frente a una única alternativa).

use super::{
    AiModelRuntime, Provider, SpamAiCategory, SpamAiConfig, SpamAiSignal,
    cliente::{AiError, AiRequest, TokenLogprob},
};
use serde_json::{Value, json};
use std::time::Instant;

/// Veredicto de un modelo.
#[derive(Debug, Clone, PartialEq)]
pub struct AiVerdict {
    /// Índice en `SpamAiConfig::categories`.
    pub category: usize,
    pub probability: f32,
    pub confidence: f32,
    /// Una por señal de `SpamAiConfig::signals`.
    pub signals: Vec<bool>,
    pub explanation: Option<String>,
    /// Identificador del modelo que respondió (el que dice el proveedor o el configurado).
    pub model: String,
}

pub fn verdict_schema(
    categories: &[SpamAiCategory],
    signals: &[SpamAiSignal],
    explanation: bool,
) -> Value {
    let mut properties = serde_json::Map::new();
    let mut required = vec![Value::from("category"), Value::from("confidence")];
    properties.insert(
        "category".into(),
        json!({"type": "string", "enum": categories.iter().map(|c| c.name.as_str()).collect::<Vec<_>>()}),
    );
    properties.insert(
        "confidence".into(),
        json!({"type": "string", "enum": ["high", "medium", "low"]}),
    );
    if !signals.is_empty() {
        let mut signal_props = serde_json::Map::new();
        for signal in signals {
            signal_props.insert(signal.name.clone(), json!({"type": "boolean"}));
        }
        properties.insert(
            "signals".into(),
            json!({
                "type": "object",
                "properties": Value::Object(signal_props),
                "required": signals.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
                "additionalProperties": false
            }),
        );
        required.push("signals".into());
    }
    if explanation {
        properties.insert("explanation".into(), json!({"type": "string"}));
        required.push("explanation".into());
    }
    json!({
        "type": "object",
        "properties": Value::Object(properties),
        "required": required,
        "additionalProperties": false
    })
}

pub fn system_prompt(
    instructions: &str,
    categories: &[SpamAiCategory],
    signals: &[SpamAiSignal],
    explanation: bool,
) -> String {
    // El esquema guarda los saltos de línea de `instructions` escapados (`\n` literal).
    let mut prompt = instructions.replace("\\n", "\n");
    prompt.push_str("\n\nCategories (choose exactly one, by name):\n");
    for category in categories {
        prompt.push_str("- ");
        prompt.push_str(&category.name);
        prompt.push_str(": ");
        prompt.push_str(&category.description);
        if !category.includes.is_empty() {
            prompt.push_str(". Examples: ");
            prompt.push_str(&category.includes.join("; "));
        }
        prompt.push('\n');
    }
    if !signals.is_empty() {
        prompt.push_str("\nSignals (answer true or false for each):\n");
        for signal in signals {
            prompt.push_str("- ");
            prompt.push_str(&signal.name);
            prompt.push_str(": ");
            prompt.push_str(&signal.question);
            prompt.push('\n');
        }
    }
    prompt.push_str(
        "\nAnswer with a single JSON object and nothing else, with these members:\n\
         - \"category\": the name of the chosen category\n\
         - \"confidence\": \"high\", \"medium\" or \"low\"\n",
    );
    if !signals.is_empty() {
        prompt.push_str("- \"signals\": an object with one boolean member per signal name\n");
    }
    if explanation {
        prompt.push_str("- \"explanation\": one short sentence explaining the verdict\n");
    }
    prompt
}

impl SpamAiConfig {
    /// Pide el veredicto a un modelo. `facts` es el registro de hechos ya serializado.
    pub async fn classify(
        &self,
        model: &AiModelRuntime,
        facts: &str,
        facts_value: &Value,
        deadline: Instant,
        session_id: u64,
    ) -> Result<AiVerdict, AiError> {
        if model.provider == Provider::TypeSafe {
            return super::typesafe::classify(self, model, facts_value, deadline, session_id).await;
        }

        // Límite de salida propio del clasificador (más bajo) salvo con razonamiento, que lo necesita entero.
        let max_tokens = if matches!(model.reasoning, registry::schema::enums::AiReasoning::Off)
            && model.provider != Provider::Anthropic
        {
            let own = if self.explanation {
                super::SPAM_MAX_OUTPUT_TOKENS_EXPLAINED
            } else {
                super::SPAM_MAX_OUTPUT_TOKENS
            };
            own.min(model.max_output_tokens)
        } else {
            model.max_output_tokens
        };

        let request = AiRequest {
            system: Some(&self.system_prompt),
            prompt: facts,
            max_tokens,
            temperature: model.temperature,
            json_schema: Some(&self.schema),
            logprobs: model.logprobs,
        };
        let response = model.generate(&request, deadline, session_id).await?;
        let reported = response
            .model
            .clone()
            .unwrap_or_else(|| model.model.clone());
        self.parse_verdict(&response.text, &response.logprobs, reported)
    }

    /// Interpreta la respuesta generativa.
    pub fn parse_verdict(
        &self,
        text: &str,
        logprobs: &[TokenLogprob],
        model: String,
    ) -> Result<AiVerdict, AiError> {
        let value =
            extract_json(text).ok_or_else(|| AiError::invalid("Invalid JSON in AI verdict"))?;
        let category_name = value
            .get("category")
            .and_then(|c| c.as_str())
            .ok_or_else(|| AiError::invalid("AI verdict without category"))?;
        let category = self
            .categories
            .iter()
            .position(|c| c.name.eq_ignore_ascii_case(category_name.trim()))
            .ok_or_else(|| {
                AiError::invalid(format!(
                    "AI verdict with unknown category {:?}",
                    super::cliente::truncate_chars(category_name, 40)
                ))
            })?;

        let verbal = match value
            .get("confidence")
            .and_then(|c| c.as_str())
            .map(|c| c.trim().to_ascii_lowercase())
            .as_deref()
        {
            Some("high") => self.verbal_high,
            Some("medium") => self.verbal_medium,
            _ => self.verbal_low,
        };

        let (probability, confidence) = match logprob_probability(logprobs, category_name) {
            Some((p, margin)) => (p, margin.unwrap_or((2.0 * p - 1.0).max(0.0))),
            None => (verbal, (2.0 * verbal - 1.0).max(0.0)),
        };

        let signals = self
            .signals
            .iter()
            .map(
                |s| match value.get("signals").and_then(|v| v.get(&s.name)) {
                    Some(Value::Bool(b)) => *b,
                    Some(Value::String(s)) => {
                        matches!(s.trim().to_ascii_lowercase().as_str(), "yes" | "true")
                    }
                    Some(Value::Number(n)) => n.as_f64().is_some_and(|n| n >= 0.5),
                    _ => false,
                },
            )
            .collect();

        Ok(AiVerdict {
            category,
            probability: probability.clamp(0.0, 1.0),
            confidence: confidence.clamp(0.0, 1.0),
            signals,
            explanation: if self.explanation {
                value
                    .get("explanation")
                    .and_then(|e| e.as_str())
                    .map(|e| super::cliente::truncate_chars(e.trim(), 300).to_string())
                    .filter(|e| !e.is_empty())
            } else {
                None
            },
            model,
        })
    }
}

/// Primer objeto JSON del texto (admite bloques ```json y texto alrededor).
pub fn extract_json(text: &str) -> Option<Value> {
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    if end < start {
        return None;
    }
    serde_json::from_str::<Value>(&text[start..=end])
        .ok()
        .filter(|v| v.is_object())
}

/// Probabilidad del nombre de categoría según los `logprobs` de sus tokens y margen en el primer token.
pub fn logprob_probability(tokens: &[TokenLogprob], category: &str) -> Option<(f32, Option<f32>)> {
    if tokens.is_empty() {
        return None;
    }
    let mut text = String::new();
    let mut spans = Vec::with_capacity(tokens.len());
    for token in tokens {
        let start = text.len();
        text.push_str(&token.token);
        spans.push((start, text.len()));
    }
    let key = text.find("\"category\"")?;
    let after_key = key + "\"category\"".len();
    let colon = after_key + text[after_key..].find(':')?;
    let value_start = colon + 1 + text[colon + 1..].find('"')? + 1;
    let value_len = text[value_start..].find('"')?;
    if value_len == 0 || !text[value_start..value_start + value_len].eq_ignore_ascii_case(category)
    {
        return None;
    }
    let value_end = value_start + value_len;

    let mut sum = 0.0f64;
    let mut first: Option<&TokenLogprob> = None;
    for (token, (start, end)) in tokens.iter().zip(spans) {
        if end > value_start && start < value_end {
            sum += token.logprob;
            if first.is_none() {
                first = Some(token);
            }
        }
    }
    let first = first?;
    let probability = sum.exp() as f32;
    let chosen = first.logprob.exp();
    let second = first
        .top
        .iter()
        .filter(|(t, _)| t != &first.token)
        .map(|(_, l)| l.exp())
        .fold(None, |acc: Option<f64>, p| {
            Some(acc.map_or(p, |a| a.max(p)))
        });
    let margin = if first.top.is_empty() {
        None
    } else {
        Some((chosen - second.unwrap_or(0.0)).max(0.0) as f32)
    };
    Some((probability, margin))
}

#[cfg(test)]
mod pruebas {
    use super::*;

    fn config() -> SpamAiConfig {
        let categories = ["legitimate", "phishing", "bulk_spam"]
            .iter()
            .map(|n| SpamAiCategory {
                name: n.to_string(),
                tag: super::super::tag_part(n),
                description: format!("{n} mail"),
                includes: vec!["x".into()],
            })
            .collect::<Vec<_>>();
        let signals = vec![SpamAiSignal {
            name: "impersonation".into(),
            tag: "IMPERSONATION".into(),
            question: "Does it?".into(),
        }];
        SpamAiConfig {
            shadow: false,
            condition: crate::expr::if_block::IfBlock::empty(
                registry::schema::prelude::ObjectType::SpamAi.singleton(),
                registry::schema::prelude::Property::Condition,
            ),
            tiers: vec![],
            system_prompt: system_prompt("Classify.", &categories, &signals, true),
            instructions: "Classify.".into(),
            schema: verdict_schema(&categories, &signals, true),
            categories,
            signals,
            explanation: true,
            bucket_high: 0.85,
            bucket_medium: 0.65,
            uncertain_below: 0.3,
            verbal_high: 0.9,
            verbal_medium: 0.7,
            verbal_low: 0.55,
            add_header: true,
            max_input_chars: 6000,
            max_links: 20,
            max_attachments: 10,
            trim_quotes: true,
            include_auth: true,
            include_links: true,
            include_attachments: true,
            deadline: std::time::Duration::from_secs(10),
            breaker_threshold: 5,
            breaker_cooldown: std::time::Duration::from_secs(60),
            learn: None,
        }
    }

    #[test]
    fn veredicto_verbal() {
        let c = config();
        let v = c
            .parse_verdict(
                "```json\n{\"category\":\"Phishing\",\"confidence\":\"high\",\"signals\":{\"impersonation\":true},\"explanation\":\"fake bank\"}\n```",
                &[],
                "m".into(),
            )
            .unwrap();
        assert_eq!(v.category, 1);
        assert!((v.probability - 0.9).abs() < 1e-6);
        assert!((v.confidence - 0.8).abs() < 1e-5);
        assert_eq!(v.signals, vec![true]);
        assert_eq!(v.explanation.as_deref(), Some("fake bank"));
        let v = c
            .parse_verdict(
                "{\"category\":\"legitimate\",\"confidence\":\"low\"}",
                &[],
                "m".into(),
            )
            .unwrap();
        assert!(v.confidence < 0.3);
        assert_eq!(v.signals, vec![false]);
        assert!(c.parse_verdict("no json", &[], "m".into()).is_err());
        assert!(
            c.parse_verdict("{\"category\":\"otra\"}", &[], "m".into())
                .is_err()
        );
    }

    #[test]
    fn veredicto_con_logprobs() {
        let c = config();
        let t = |token: &str, lp: f64, top: &[(&str, f64)]| TokenLogprob {
            token: token.into(),
            logprob: lp,
            top: top.iter().map(|(a, b)| (a.to_string(), *b)).collect(),
        };
        let tokens = vec![
            t("{\"", 0.0, &[]),
            t("category", 0.0, &[]),
            t("\":\"", 0.0, &[]),
            t("bulk", -0.1, &[("bulk", -0.1), ("phishing", -2.5)]),
            t("_spam", -0.02, &[]),
            t("\",\"", 0.0, &[]),
            t("confidence\":\"low\"}", 0.0, &[]),
        ];
        let (p, margin) = logprob_probability(&tokens, "bulk_spam").unwrap();
        assert!((p - (-0.12f64).exp() as f32).abs() < 1e-5);
        let expected = ((-0.1f64).exp() - (-2.5f64).exp()) as f32;
        assert!((margin.unwrap() - expected).abs() < 1e-5);
        let text: String = tokens.iter().map(|t| t.token.as_str()).collect();
        let v = c.parse_verdict(&text, &tokens, "m".into()).unwrap();
        assert_eq!(v.category, 2);
        assert!(v.probability > 0.88);
        assert!(
            v.confidence > 0.8,
            "la confianza viene de los logprobs, no del «low» verbal"
        );
        // Sin el nombre en los tokens no hay probabilidad por logprobs.
        assert!(logprob_probability(&tokens, "phishing").is_none());
    }

    #[test]
    fn esquema_e_instrucciones() {
        let c = config();
        assert_eq!(
            c.schema["properties"]["category"]["enum"],
            json!(["legitimate", "phishing", "bulk_spam"])
        );
        assert_eq!(
            c.schema["required"],
            json!(["category", "confidence", "signals", "explanation"])
        );
        assert!(c.system_prompt.starts_with("Classify."));
        assert!(
            c.system_prompt
                .contains("- phishing: phishing mail. Examples: x")
        );
        assert!(c.system_prompt.contains("- impersonation: Does it?"));
    }
}
