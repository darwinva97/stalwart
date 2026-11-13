/*
 * SPDX-FileCopyrightText: 2026 bezenti
 *
 * SPDX-License-Identifier: AGPL-3.0-only
 */

//! Función Sieve `llm_prompt(modelo, prompt, temperatura)` (extensión `vnd.stalwart.expressions`).
//!
//! Según la documentación pública: el modelo se elige por su `name` (con un único modelo, `""` lo elige); el
//! prompt va como texto sin instrucciones de sistema y la respuesta vuelve como texto; la temperatura (0–1) se
//! recorta al rango y, si no es un número, manda la del modelo; el resto (`maxOutputTokens`, `reasoning`,
//! `extraParameters`, `timeout`, `maxConcurrentRequests`, `rateLimit`) sale de `x:AiModel`. Cualquier fallo
//! devuelve `false` (modelo desconocido, sin permiso, tiempo agotado, límite, cortacircuitos abierto, error del
//! proveedor). En scripts de usuario hace falta el permiso `interactAi`; sin él se registra
//! `security.unauthorized`.
//!
//! Decisiones bezenti: el prompt se recorta a `MAX_PROMPT_CHARS` caracteres; en Anthropic la temperatura del
//! script solo se envía si el modelo tiene temperatura configurada (los modelos actuales la rechazan).

use super::{
    Provider,
    cliente::{AiErrorKind, AiRequest, MODEL_BREAKER_COOLDOWN, MODEL_BREAKER_THRESHOLD},
};
use crate::scripts::plugins::PluginContext;
use registry::schema::enums::Permission;
use sieve::runtime::Variable;
use std::time::Instant;
use trc::{AiEvent, SecurityEvent};

/// Tamaño máximo del prompt de un script (decisión bezenti).
pub const MAX_PROMPT_CHARS: usize = 100_000;

pub async fn llm_prompt(ctx: PluginContext<'_>) -> trc::Result<Variable<'static>> {
    let session_id = ctx.session_id;

    // Permiso: solo se exige en scripts de usuario (los de sistema son de confianza).
    if let Some(token) = ctx.access_token
        && !token.has_permission(Permission::InteractAi)
    {
        trc::event!(
            Security(SecurityEvent::Unauthorized),
            SpanId = session_id,
            AccountId = token.account_id(),
            Details = "llm_prompt requires the interactAi permission",
        );
        return Ok(false.into());
    }

    let name = ctx.arguments[0].to_string();
    let Some(model) = ctx.server.core.ai.model_by_name(name.as_ref()).cloned() else {
        trc::event!(
            Ai(AiEvent::ApiError),
            SpanId = session_id,
            Model = name.to_string(),
            Reason = "Unknown AI model",
        );
        return Ok(false.into());
    };

    if model.provider == Provider::TypeSafe {
        trc::event!(
            Ai(AiEvent::ApiError),
            SpanId = session_id,
            Model = model.name.clone(),
            Reason = "Operation not supported by this model",
        );
        return Ok(false.into());
    }

    if model.breaker.is_open() {
        trc::event!(
            Ai(AiEvent::ApiError),
            SpanId = session_id,
            Model = model.name.clone(),
            Reason = "AI model temporarily disabled by the circuit breaker",
        );
        return Ok(false.into());
    }

    let prompt = ctx.arguments[1].to_string();
    let prompt = super::cliente::truncate_chars(prompt.as_ref(), MAX_PROMPT_CHARS);
    let temperature = match &ctx.arguments[2] {
        Variable::Float(f) if f.is_finite() => Some(f.clamp(0.0, 1.0)),
        Variable::Integer(i) => Some((*i as f64).clamp(0.0, 1.0)),
        Variable::String(s) => s.trim().parse::<f64>().ok().map(|f| f.clamp(0.0, 1.0)),
        _ => None,
    };
    let temperature = match model.provider {
        Provider::Anthropic if model.temperature.is_none() => None,
        _ => temperature.or(model.temperature),
    };

    let request = AiRequest {
        system: None,
        prompt,
        max_tokens: model.max_output_tokens,
        temperature,
        json_schema: None,
        logprobs: false,
    };
    let deadline = Instant::now() + model.timeout;
    let result = model.generate(&request, deadline, session_id).await;
    model.record_breaker(
        &model.breaker,
        result.as_ref().map(|_| ()),
        MODEL_BREAKER_THRESHOLD,
        MODEL_BREAKER_COOLDOWN,
        session_id,
    );

    match result {
        Ok(response) => Ok(Variable::from(response.text)),
        Err(err) => {
            if err.kind != AiErrorKind::BreakerOpen {
                model.log_error(&err, session_id);
            }
            Ok(false.into())
        }
    }
}
