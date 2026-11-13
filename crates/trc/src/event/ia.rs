/*
 * SPDX-FileCopyrightText: 2026 bezenti
 *
 * SPDX-License-Identifier: AGPL-3.0-only
 */

//! Edición bezenti, fase 4: eventos, métricas y claves de traza de la IA.
//!
//! Los nombres (`ai.request`, `ai.api-error`…), etiquetas y explicaciones vienen del esquema público
//! (`resources/schema/schema.json.gz`, enums `EventType`, `MetricType` y `Key`). Los identificadores
//! numéricos son decisión bezenti: se usan huecos libres de la numeración del código limpio (eventos
//! 609–614, métricas 328–335, claves 87–95) para no mover ningún identificador existente. Ver
//! `bezenti/especificaciones/ia.md`, §6.

use crate::{EventType, Level, MetricType};

/// Eventos `ai.*` (esquema, enum `EventType`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u16)]
pub enum AiEvent {
    /// `ai.llm-response`: respuesta recibida del modelo (nivel traza).
    LlmResponse = 609,
    /// `ai.api-error`: falló una petición al proveedor.
    ApiError = 610,
    /// `ai.request`: petición completada (modelo, latencia, tokens, motivo de fin, id del proveedor).
    Request = 611,
    /// `ai.spam-verdict`: veredicto del clasificador antispam con IA.
    SpamVerdict = 612,
    /// `ai.spam-skipped`: el clasificador no se ejecutó (motivo en `reason`).
    SpamSkipped = 613,
    /// `ai.breaker-open`: el cortacircuitos de un modelo se abrió.
    BreakerOpen = 614,
}

pub(crate) const AI_EVENTS: [AiEvent; 6] = [
    AiEvent::LlmResponse,
    AiEvent::ApiError,
    AiEvent::Request,
    AiEvent::SpamVerdict,
    AiEvent::SpamSkipped,
    AiEvent::BreakerOpen,
];

impl AiEvent {
    pub const fn to_id(&self) -> u16 {
        *self as u16
    }

    pub fn from_id(id: u16) -> Option<Self> {
        AI_EVENTS.iter().copied().find(|e| e.to_id() == id)
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            AiEvent::LlmResponse => "ai.llm-response",
            AiEvent::ApiError => "ai.api-error",
            AiEvent::Request => "ai.request",
            AiEvent::SpamVerdict => "ai.spam-verdict",
            AiEvent::SpamSkipped => "ai.spam-skipped",
            AiEvent::BreakerOpen => "ai.breaker-open",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        AI_EVENTS.iter().copied().find(|e| e.as_str() == value)
    }

    pub fn level(&self) -> Level {
        match self {
            AiEvent::LlmResponse => Level::Trace,
            AiEvent::ApiError | AiEvent::BreakerOpen => Level::Warn,
            AiEvent::Request | AiEvent::SpamSkipped => Level::Debug,
            AiEvent::SpamVerdict => Level::Info,
        }
    }

    pub fn description(&self) -> &'static str {
        match self {
            AiEvent::LlmResponse => "LLM response",
            AiEvent::ApiError => "AI API error",
            AiEvent::Request => "AI request completed",
            AiEvent::SpamVerdict => "AI spam verdict",
            AiEvent::SpamSkipped => "AI spam classifier skipped",
            AiEvent::BreakerOpen => "AI model circuit breaker open",
        }
    }

    pub fn message(&self) -> &'static str {
        match self {
            AiEvent::ApiError => "AI API error",
            AiEvent::BreakerOpen => "AI model unavailable",
            _ => "Internal Server Error",
        }
    }

    /// Nombre OpenTelemetry del contador del evento (solo los que el esquema da como métrica).
    pub fn otel_metric_name(&self) -> &'static str {
        match self {
            AiEvent::ApiError => "stalwart.ai.api_error",
            AiEvent::Request => "stalwart.ai.request",
            AiEvent::SpamVerdict => "stalwart.ai.spam_verdict",
            AiEvent::SpamSkipped => "stalwart.ai.spam_skipped",
            AiEvent::BreakerOpen => "stalwart.ai.breaker_open",
            AiEvent::LlmResponse => "",
        }
    }

    pub fn prom_metric_name(&self) -> &'static str {
        match self {
            AiEvent::ApiError => "stalwart_ai_api_error_total",
            AiEvent::Request => "stalwart_ai_request_total",
            AiEvent::SpamVerdict => "stalwart_ai_spam_verdict_total",
            AiEvent::SpamSkipped => "stalwart_ai_spam_skipped_total",
            AiEvent::BreakerOpen => "stalwart_ai_breaker_open_total",
            AiEvent::LlmResponse => "",
        }
    }
}

impl From<AiEvent> for EventType {
    fn from(event: AiEvent) -> Self {
        EventType::Ai(event)
    }
}

impl AiEvent {
    pub fn into_err(self) -> crate::Error {
        EventType::Ai(self).into_err()
    }
}

/// Métricas `ai.*` (esquema, enum `MetricType`): (métrica, nombre, id, evento contado o `usize::MAX`).
pub(crate) const AI_METRICS: [(MetricType, &str, u16, usize); 8] = [
    (
        MetricType::AiRequestTime,
        "ai.request-time",
        328,
        usize::MAX,
    ),
    (
        MetricType::AiInputTokens,
        "ai.input-tokens",
        329,
        usize::MAX,
    ),
    (
        MetricType::AiOutputTokens,
        "ai.output-tokens",
        330,
        usize::MAX,
    ),
    (MetricType::AiApiError, "ai.api-error", 331, 610),
    (MetricType::AiRequest, "ai.request", 332, 611),
    (MetricType::AiSpamVerdict, "ai.spam-verdict", 333, 612),
    (MetricType::AiSpamSkipped, "ai.spam-skipped", 334, 613),
    (MetricType::AiBreakerOpen, "ai.breaker-open", 335, 614),
];

pub(crate) fn metric_entry(
    metric: &MetricType,
) -> Option<&'static (MetricType, &'static str, u16, usize)> {
    AI_METRICS.iter().find(|(m, ..)| m == metric)
}

pub(crate) fn metric_parse(value: &str) -> Option<MetricType> {
    AI_METRICS
        .iter()
        .find(|(_, name, ..)| *name == value)
        .map(|(m, ..)| *m)
}

pub(crate) fn metric_from_id(id: u16) -> Option<MetricType> {
    AI_METRICS
        .iter()
        .find(|(_, _, mid, _)| *mid == id)
        .map(|(m, ..)| *m)
}

pub(crate) fn metric_description(metric: &MetricType) -> &'static str {
    match metric {
        MetricType::AiRequestTime => "AI request duration",
        MetricType::AiInputTokens => "AI input tokens per request",
        MetricType::AiOutputTokens => "AI output tokens per request",
        MetricType::AiApiError => "AI API error",
        MetricType::AiRequest => "AI request completed",
        MetricType::AiSpamVerdict => "AI spam verdict",
        MetricType::AiSpamSkipped => "AI spam classifier skipped",
        MetricType::AiBreakerOpen => "AI model circuit breaker open",
        _ => "",
    }
}

/// (unidad, nombre OTel, unidad OTel, nombre Prometheus, unidad Prometheus) de los histogramas de IA.
pub(crate) fn metric_names(
    metric: &MetricType,
) -> Option<(
    &'static str,
    &'static str,
    &'static str,
    &'static str,
    &'static str,
)> {
    match metric {
        MetricType::AiRequestTime => Some((
            "milliseconds",
            "stalwart.ai.request_time",
            "s",
            "stalwart_ai_request_time_seconds",
            "seconds",
        )),
        MetricType::AiInputTokens => Some((
            "tokens",
            "stalwart.ai.input_tokens",
            "{token}",
            "stalwart_ai_input_tokens",
            "",
        )),
        MetricType::AiOutputTokens => Some((
            "tokens",
            "stalwart.ai.output_tokens",
            "{token}",
            "stalwart_ai_output_tokens",
            "",
        )),
        MetricType::AiApiError
        | MetricType::AiRequest
        | MetricType::AiSpamVerdict
        | MetricType::AiSpamSkipped
        | MetricType::AiBreakerOpen => Some(("count", "", "", "", "")),
        _ => None,
    }
}

#[cfg(test)]
mod pruebas {
    use super::*;
    use crate::Key;

    #[test]
    fn eventos_ida_y_vuelta() {
        for event in AI_EVENTS {
            let et = EventType::Ai(event);
            assert_eq!(EventType::parse(et.as_str()), Some(et), "{}", et.as_str());
            assert_eq!(EventType::from_id(et.to_id()), Some(et));
            assert!((et.to_id() as usize) < crate::TOTAL_EVENT_COUNT);
            assert!(EventType::variants().contains(&et));
        }
        // Los ids elegidos no pisan ningún evento del código limpio.
        let mut ids: Vec<u16> = EventType::variants().iter().map(|e| e.to_id()).collect();
        ids.sort_unstable();
        let total = ids.len();
        ids.dedup();
        assert_eq!(total, ids.len(), "ids de evento repetidos");
    }

    #[test]
    fn metricas_ida_y_vuelta() {
        for (metric, name, id, event_id) in AI_METRICS {
            assert_eq!(MetricType::parse(name), Some(metric));
            assert_eq!(metric.as_str(), name);
            assert_eq!(metric.to_id(), id);
            assert_eq!(MetricType::from_id(id), Some(metric));
            assert_eq!(metric.event_id(), event_id);
            assert!((id as usize) < crate::TOTAL_METRIC_COUNT);
            assert!(MetricType::variants().contains(&metric));
        }
        let mut ids: Vec<u16> = MetricType::variants().iter().map(|e| e.to_id()).collect();
        ids.sort_unstable();
        let total = ids.len();
        ids.dedup();
        assert_eq!(total, ids.len(), "ids de métrica repetidos");
    }

    #[test]
    fn claves_ida_y_vuelta() {
        for (key, name) in [
            (Key::Model, "model"),
            (Key::InputTokens, "inputTokens"),
            (Key::OutputTokens, "outputTokens"),
            (Key::CachedTokens, "cachedTokens"),
            (Key::FinishReason, "finishReason"),
            (Key::Category, "category"),
            (Key::Probability, "probability"),
            (Key::Confidence, "confidence"),
            (Key::Tier, "tier"),
        ] {
            assert_eq!(key.as_str(), name);
            assert_eq!(Key::parse(name), Some(key));
            assert_eq!(Key::from_id(key.to_id()), Some(key));
        }
    }
}

/// Claves de traza de la IA (esquema, enum `Key`).
pub(crate) const AI_KEYS: [(crate::Key, &str); 9] = [
    (crate::Key::Model, "model"),
    (crate::Key::InputTokens, "inputTokens"),
    (crate::Key::OutputTokens, "outputTokens"),
    (crate::Key::CachedTokens, "cachedTokens"),
    (crate::Key::FinishReason, "finishReason"),
    (crate::Key::Category, "category"),
    (crate::Key::Probability, "probability"),
    (crate::Key::Confidence, "confidence"),
    (crate::Key::Tier, "tier"),
];

pub(crate) fn key_as_str(key: &crate::Key) -> &'static str {
    AI_KEYS
        .iter()
        .find(|(k, _)| k == key)
        .map_or("", |(_, name)| name)
}

pub(crate) fn key_parse(value: &str) -> Option<crate::Key> {
    AI_KEYS
        .iter()
        .find(|(_, name)| *name == value)
        .map(|(k, _)| *k)
}

pub(crate) fn key_from_id(id: u16) -> Option<crate::Key> {
    AI_KEYS
        .iter()
        .find(|(k, _)| k.to_id() == id)
        .map(|(k, _)| *k)
}
