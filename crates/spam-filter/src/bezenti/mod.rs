/*
 * SPDX-FileCopyrightText: 2026 bezenti
 *
 * SPDX-License-Identifier: AGPL-3.0-only
 */

//! Edición bezenti, fase 4: etapa del clasificador antispam con IA (`x:SpamAi`).
//!
//! Se ejecuta después del clasificador estadístico y de las reglas que no dependen de etiquetas `AI_`, con la
//! puntuación provisional, y antes de las reglas que sí dependen de ellas (documentación pública). Pasos:
//!
//! 1. Compuerta (`AiGate`), en este orden: `authenticated`, `decided`, `condition`, `unavailable`,
//!    `no-content`; si no aplica ninguno, `run`. El motivo se registra en `ai.spam-skipped`.
//! 2. Registro de hechos (`hechos.rs`).
//! 3. Niveles (`tiers`) en orden, todos dentro del mismo plazo (`deadline`): un nivel solo se consulta si el
//!    anterior falló o fue poco concluyente (confianza bajo `escalateBelow`, incierto, probabilidad en el cubo
//!    bajo o categoría en `escalateOn`). Vale el veredicto del último nivel que respondió.
//! 4. Etiquetas `AI_<CATEGORÍA>_HIGH/MEDIUM/LOW` (o `AI_UNCERTAIN`) y `AI_<SEÑAL>`; si ningún modelo da
//!    veredicto, `AI_ERROR`. En modo `Shadow` no se añaden etiquetas ni se aprende.
//! 5. Cabecera `X-Spam-AI`, evento `ai.spam-verdict` y, si procede, muestra de entrenamiento.
//!
//! Un fallo del modelo nunca bloquea ni pierde el correo: el mensaje sigue el camino clásico.

pub mod hechos;

use crate::{
    AiGate, SpamFilterContext,
    analysis::score::ScoreAccumulator,
    modules::expression::{ScoreVariables, SpamFilterResolver, StringResolver},
};
use common::{
    Server,
    bezenti::ia::{
        SpamAiConfig,
        clasificacion::AiVerdict,
        cliente::{AiError, AiErrorKind},
    },
    config::mailstore::spamfilter::{Location, SpamFilterAction},
};
use compact_str::CompactString;
use std::{fmt::Write, future::Future, sync::Arc, time::Instant};
use trc::AiEvent;

pub trait SpamFilterAnalyzeAi: Sync + Send {
    fn spam_filter_analyze_ai(
        &self,
        ctx: &mut SpamFilterContext<'_>,
        acc: &mut ScoreAccumulator,
    ) -> impl Future<Output = ()> + Send;
}

impl SpamFilterAnalyzeAi for Server {
    async fn spam_filter_analyze_ai(
        &self,
        ctx: &mut SpamFilterContext<'_>,
        acc: &mut ScoreAccumulator,
    ) {
        let Some(config) = self.core.ai.spam.clone() else {
            return;
        };
        let span_id = ctx.input.span_id;

        let gate = gate(self, ctx, acc, &config).await;
        ctx.result.ai.gate = Some(gate);
        if gate != AiGate::Run {
            trc::event!(
                Ai(AiEvent::SpamSkipped),
                SpanId = span_id,
                Reason = gate.as_str(),
            );
            return;
        }

        let facts_value = hechos::build(ctx, &config);
        let facts = facts_value.to_string();
        ctx.result.ai.facts = Some(facts.clone());

        let deadline = Instant::now() + config.deadline;
        let outcome = match tokio::time::timeout(
            config.deadline,
            run_tiers(&config, &facts, &facts_value, deadline, span_id),
        )
        .await
        {
            Ok(outcome) => outcome,
            Err(_) => TierOutcome {
                verdict: None,
                earlier: Vec::new(),
                error: Some((1, AiError::new(AiErrorKind::Timeout, "Request timed out"))),
            },
        };

        apply(self, ctx, acc, &config, outcome);
    }
}

async fn gate(
    server: &Server,
    ctx: &mut SpamFilterContext<'_>,
    acc: &ScoreAccumulator,
    config: &SpamAiConfig,
) -> AiGate {
    if ctx.input.authenticated_as.is_some() {
        return AiGate::Authenticated;
    }
    if acc.decision().is_some() {
        return AiGate::Decided;
    }

    let scores = ScoreVariables {
        score: acc.score(),
        probability: acc.probability().unwrap_or(-1.0),
        threshold: server.core.spam.scores.spam_threshold,
    };
    let mut arena = std::mem::take(ctx.arena.get_mut());
    let item = StringResolver("");
    let resolver = SpamFilterResolver::new(
        &ctx.input,
        &ctx.output,
        &ctx.result.tags,
        &item,
        Location::BodyText,
    )
    .with_scores(scores);
    let passes = server
        .eval_if::<bool, _>(&config.condition, &resolver, &mut arena, ctx.input.span_id)
        .await
        .unwrap_or(false);
    arena.reset();
    *ctx.arena.get_mut() = arena;
    if !passes {
        return AiGate::Condition;
    }

    if config.tiers.is_empty() || config.tiers.iter().all(|t| t.breaker.is_open()) {
        return AiGate::Unavailable;
    }
    if !hechos::has_content(ctx) {
        return AiGate::NoContent;
    }
    AiGate::Run
}

struct TierOutcome {
    /// (nivel, veredicto) del último nivel que respondió (niveles numerados desde 1).
    verdict: Option<(usize, AiVerdict)>,
    /// (nivel, categoría, probabilidad) de los niveles anteriores que respondieron y escalaron.
    earlier: Vec<(usize, String, f32)>,
    /// Último error (nivel, error) si algún nivel falló.
    error: Option<(usize, AiError)>,
}

async fn run_tiers(
    config: &Arc<SpamAiConfig>,
    facts: &str,
    facts_value: &serde_json::Value,
    deadline: Instant,
    span_id: u64,
) -> TierOutcome {
    let mut outcome = TierOutcome {
        verdict: None,
        earlier: Vec::new(),
        error: None,
    };

    for (idx, tier) in config.tiers.iter().enumerate() {
        let number = idx + 1;
        if tier.breaker.is_open() {
            continue;
        }
        if Instant::now() >= deadline {
            outcome.error = Some((
                number,
                AiError::new(AiErrorKind::Timeout, "Request timed out"),
            ));
            break;
        }
        let result = config
            .classify(&tier.model, facts, facts_value, deadline, span_id)
            .await;
        tier.model.record_breaker(
            &tier.breaker,
            result.as_ref().map(|_| ()),
            config.breaker_threshold,
            config.breaker_cooldown,
            span_id,
        );
        match result {
            Ok(verdict) => {
                let category = &config.categories[verdict.category];
                let uncertain = verdict.confidence < config.uncertain_below;
                let escalate = verdict.confidence < tier.escalate_below
                    || uncertain
                    || verdict.probability < config.bucket_medium
                    || tier
                        .escalate_on
                        .contains(&category.name.to_ascii_lowercase());
                if let Some((n, previous)) = outcome.verdict.take() {
                    outcome.earlier.push((
                        n,
                        config.categories[previous.category].name.clone(),
                        previous.probability,
                    ));
                }
                outcome.verdict = Some((number, verdict));
                if !escalate {
                    break;
                }
            }
            Err(err) => {
                tier.model.log_error(&err, span_id);
                outcome.error = Some((number, err));
            }
        }
    }
    outcome
}

fn apply(
    server: &Server,
    ctx: &mut SpamFilterContext<'_>,
    acc: &mut ScoreAccumulator,
    config: &SpamAiConfig,
    outcome: TierOutcome,
) {
    let span_id = ctx.input.span_id;
    let scores = &server.core.spam.lists.scores;

    let Some((tier, verdict)) = outcome.verdict else {
        // Ningún modelo dio veredicto.
        let Some((tier, err)) = outcome.error else {
            // Todos los niveles se saltaron por el cortacircuitos: no se añade ninguna etiqueta.
            ctx.result.ai.gate = Some(AiGate::Unavailable);
            trc::event!(
                Ai(AiEvent::SpamSkipped),
                SpanId = span_id,
                Reason = AiGate::Unavailable.as_str(),
            );
            return;
        };
        let mut value = format!(
            "error; tier={tier}; error=\"{}\"",
            quoted(&err.message, 200)
        );
        if config.shadow {
            value.push_str("; shadow");
        } else {
            let tag = CompactString::const_new("AI_ERROR");
            acc.add(&tag, scores);
            ctx.result.tags.insert(tag);
        }
        set_header(ctx, config, value);
        return;
    };

    let category = &config.categories[verdict.category];
    let uncertain = verdict.confidence < config.uncertain_below;
    let mut tags: Vec<CompactString> = Vec::with_capacity(1 + verdict.signals.len());
    let category_tag = if uncertain {
        CompactString::const_new("AI_UNCERTAIN")
    } else {
        let bucket = if verdict.probability >= config.bucket_high {
            "HIGH"
        } else if verdict.probability >= config.bucket_medium {
            "MEDIUM"
        } else {
            "LOW"
        };
        CompactString::from(format!("AI_{}_{bucket}", category.tag))
    };
    tags.push(category_tag.clone());
    for (signal, present) in config.signals.iter().zip(&verdict.signals) {
        if *present {
            tags.push(CompactString::from(format!("AI_{}", signal.tag)));
        }
    }

    // Cabecera X-Spam-AI.
    let mut value = String::with_capacity(128);
    let _ = write!(
        value,
        "{}; p={:.2}; confidence={:.2}; model={}; tier={tier}",
        category.name,
        verdict.probability,
        verdict.confidence,
        token(&verdict.model),
    );
    if uncertain {
        value.push_str("; uncertain");
    }
    for (n, name, p) in &outcome.earlier {
        let _ = write!(value, "; tier{n}={name}/{p:.2}");
    }
    if config.shadow {
        value.push_str("; shadow");
    }
    if let Some(reason) = &verdict.explanation {
        let _ = write!(value, "; reason=\"{}\"", quoted(reason, 200));
    }

    trc::event!(
        Ai(AiEvent::SpamVerdict),
        SpanId = span_id,
        Category = category.name.clone(),
        Probability = verdict.probability as f64,
        Confidence = verdict.confidence as f64,
        Tier = tier as u64,
        Model = verdict.model.clone(),
        Tag = tags
            .iter()
            .map(|t| trc::Value::from(t.to_string()))
            .collect::<Vec<_>>(),
        Result = if config.shadow { "shadow" } else { "automatic" },
    );

    if !config.shadow {
        for tag in &tags {
            acc.add(tag, scores);
            ctx.result.tags.insert(tag.clone());
        }

        // Aprendizaje a partir de veredictos seguros (documentación pública, «Learning from Verdicts»).
        if let Some((min_probability, sample_rate)) = config.learn
            && !uncertain
            && verdict.probability >= min_probability
            && ctx
                .result
                .classifier_probability()
                .is_some_and(|p| p > 0.15 && p < 0.85)
            && ctx.input.authenticated_as.is_none()
            && !ctx.result.tags.contains("TRUSTED_DOMAIN")
            && let Some(SpamFilterAction::Allow(score)) = scores.get(&category_tag)
            && *score != 0.0
            && store::rand::random::<f32>() < sample_rate
        {
            ctx.result.ai.train_spam = Some(*score > 0.0);
        }
    }

    set_header(ctx, config, value);
}

fn set_header(ctx: &mut SpamFilterContext<'_>, config: &SpamAiConfig, value: String) {
    if config.add_header {
        ctx.result.ai.header = Some(format!("X-Spam-AI: {value}\r\n"));
    }
    ctx.result.ai.verdict = Some(value);
}

/// Texto entre comillas para la cabecera: sin saltos de línea ni controles, comillas escapadas y recortado.
fn quoted(text: &str, max: usize) -> String {
    let mut out = String::with_capacity(text.len().min(max) + 8);
    for c in text.chars().take(max) {
        match c {
            '"' | '\\' => {
                out.push('\\');
                out.push(c);
            }
            c if c.is_control() => out.push(' '),
            c => out.push(c),
        }
    }
    out
}

/// Identificador de modelo seguro para la cabecera.
fn token(text: &str) -> String {
    text.chars()
        .take(80)
        .map(|c| {
            if c.is_ascii_alphanumeric() || "-._:/@+".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod pruebas {
    use super::*;

    #[test]
    fn cabecera_segura() {
        assert_eq!(quoted("a \"b\"\r\nc", 100), "a \\\"b\\\"  c");
        assert_eq!(token("qwen3:8b; x=y"), "qwen3:8b__x_y");
    }
}
