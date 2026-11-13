/*
 * SPDX-FileCopyrightText: 2026 bezenti
 *
 * SPDX-License-Identifier: AGPL-3.0-only
 */

//! Transporte común de los clientes de IA.
//!
//! - Tiempo de espera total por llamada (`timeout` del modelo para `llm_prompt`, `deadline` de `x:SpamAi`
//!   para el clasificador), que cubre la cola de concurrencia, la espera del límite de ritmo y un reintento
//!   (documentación pública). Conexión limitada a 10 s.
//! - Un reintento con espera (o la de `Retry-After` si cabe en el plazo) ante errores de red, 408, 409, 429,
//!   5xx y 529.
//! - `maxConcurrentRequests` (semáforo) y `rateLimit` (peticiones repartidas uniformemente en el periodo).
//! - Cortacircuitos: se abre tras N fallos seguidos o al instante con 401/402/403 o un error de cuota o saldo.
//! - Telemetría: `ai.request` (modelo, latencia, tokens, motivo de fin, id del proveedor), `ai.llm-response`
//!   (texto, nivel traza), `ai.api-error` y `ai.breaker-open`. Nunca se registran claves ni cabeceras.

use super::{AiModelRuntime, Provider};
use parking_lot::Mutex;
use std::{
    sync::atomic::{AtomicU32, AtomicU64, Ordering},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use trc::AiEvent;

/// Petición genérica a un modelo generativo.
pub struct AiRequest<'a> {
    pub system: Option<&'a str>,
    pub prompt: &'a str,
    pub max_tokens: u64,
    pub temperature: Option<f64>,
    /// Esquema JSON para salida estructurada (clasificador), o `None` para texto libre.
    pub json_schema: Option<&'a serde_json::Value>,
    pub logprobs: bool,
}

/// Probabilidad logarítmica de un token de la respuesta y sus alternativas más probables.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TokenLogprob {
    pub token: String,
    pub logprob: f64,
    pub top: Vec<(String, f64)>,
}

#[derive(Debug, Clone, Default)]
pub struct AiResponse {
    pub text: String,
    pub logprobs: Vec<TokenLogprob>,
    /// Modelo que dice el proveedor que respondió.
    pub model: Option<String>,
    pub finish_reason: Option<String>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_tokens: u64,
    pub request_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AiErrorKind {
    /// Se acabó el tiempo (o el plazo del clasificador).
    Timeout,
    /// Error de red o de conexión.
    Network,
    /// Respuesta HTTP de error.
    Http(u16),
    /// Clave inválida, sin permiso, sin saldo o sin cuota: abre el cortacircuitos al instante.
    Auth(u16),
    /// Límite de concurrencia o de ritmo alcanzado antes del plazo.
    Limit,
    /// Respuesta que no se puede interpretar (JSON inválido, sin texto, rechazo…).
    InvalidResponse,
    /// El modelo no admite la operación (TypeSafe en `llm_prompt`).
    Unsupported,
    /// El cortacircuitos del modelo está abierto.
    BreakerOpen,
}

#[derive(Debug, Clone)]
pub struct AiError {
    pub kind: AiErrorKind,
    pub message: String,
}

impl AiError {
    pub fn new(kind: AiErrorKind, message: impl Into<String>) -> Self {
        AiError {
            kind,
            message: message.into(),
        }
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new(AiErrorKind::InvalidResponse, message)
    }

    fn is_retryable(&self) -> bool {
        match self.kind {
            AiErrorKind::Network => true,
            AiErrorKind::Http(status) => {
                matches!(status, 408 | 409 | 429 | 500 | 502 | 503 | 504 | 529)
            }
            _ => false,
        }
    }

    /// Errores que abren el cortacircuitos sin esperar al umbral (documentación pública).
    pub fn opens_breaker_now(&self) -> bool {
        matches!(self.kind, AiErrorKind::Auth(_))
    }

    /// Cuenta como fallo del modelo para el cortacircuitos.
    pub fn counts_as_failure(&self) -> bool {
        !matches!(
            self.kind,
            AiErrorKind::Unsupported | AiErrorKind::BreakerOpen
        )
    }
}

impl std::fmt::Display for AiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// Reparte las peticiones uniformemente en el periodo (`rateLimit`: `count` por `period`).
pub struct RateLimiter {
    interval: Duration,
    next: Mutex<Option<Instant>>,
}

impl RateLimiter {
    pub fn new(count: u64, period: Duration) -> Self {
        RateLimiter {
            interval: period / (count.clamp(1, u32::MAX as u64) as u32),
            next: Mutex::new(None),
        }
    }

    /// Reserva el siguiente hueco; `None` si no llega antes de `deadline`.
    fn reserve(&self, deadline: Instant) -> Option<Instant> {
        let now = Instant::now();
        let mut next = self.next.lock();
        let slot = next.map_or(now, |n| n.max(now));
        if slot > deadline {
            return None;
        }
        *next = Some(slot + self.interval);
        Some(slot)
    }
}

/// Cortacircuitos: tras `threshold` fallos seguidos (o un error de credenciales/saldo) se deja de consultar el
/// modelo durante `cooldown`.
#[derive(Default)]
pub struct Breaker {
    failures: AtomicU32,
    open_until_ms: AtomicU64,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

impl Breaker {
    pub fn is_open(&self) -> bool {
        self.open_until_ms.load(Ordering::Relaxed) > now_ms()
    }

    pub fn success(&self) {
        self.failures.store(0, Ordering::Relaxed);
    }

    /// Registra un fallo; devuelve `true` si el cortacircuitos acaba de abrirse.
    pub fn failure(&self, threshold: u32, cooldown: Duration, immediate: bool) -> bool {
        let failures = self.failures.fetch_add(1, Ordering::Relaxed) + 1;
        if immediate || failures >= threshold {
            self.failures.store(0, Ordering::Relaxed);
            self.open_until_ms
                .store(now_ms() + cooldown.as_millis() as u64, Ordering::Relaxed);
            true
        } else {
            false
        }
    }
}

/// Valores del cortacircuitos propio de cada modelo (documentación pública: 5 fallos, 60 s).
pub const MODEL_BREAKER_THRESHOLD: u32 = 5;
pub const MODEL_BREAKER_COOLDOWN: Duration = Duration::from_secs(60);

/// Respuesta HTTP en crudo de un proveedor.
pub struct RawResponse {
    pub body: Vec<u8>,
}

impl AiModelRuntime {
    /// Envía `body` (JSON) al punto de la API del modelo respetando límites, plazo y un reintento.
    pub async fn post_json(
        &self,
        body: &serde_json::Value,
        deadline: Instant,
        session_id: u64,
    ) -> Result<(RawResponse, Duration), AiError> {
        let started = Instant::now();

        // Concurrencia: se espera un hueco hasta el plazo.
        let remaining = deadline.saturating_duration_since(Instant::now());
        let _permit = match tokio::time::timeout(remaining, self.concurrency.acquire()).await {
            Ok(Ok(permit)) => permit,
            _ => {
                return Err(AiError::new(
                    AiErrorKind::Limit,
                    "Concurrency or rate limit reached",
                ));
            }
        };

        let body = serde_json::to_vec(body)
            .map_err(|err| AiError::invalid(format!("Failed to serialize request: {err}")))?;

        let mut attempt = 0;
        loop {
            attempt += 1;

            // Ritmo: peticiones repartidas uniformemente en el periodo.
            if let Some(limiter) = &self.rate_limiter {
                match limiter.reserve(deadline) {
                    Some(slot) => {
                        let wait = slot.saturating_duration_since(Instant::now());
                        if !wait.is_zero() {
                            tokio::time::sleep(wait).await;
                        }
                    }
                    None => {
                        return Err(AiError::new(
                            AiErrorKind::Limit,
                            "Concurrency or rate limit reached",
                        ));
                    }
                }
            }

            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(AiError::new(AiErrorKind::Timeout, "Request timed out"));
            }

            let result = self
                .client
                .post(&self.url)
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .timeout(remaining)
                .body(body.clone())
                .send()
                .await;

            let (err, retry_after) = match result {
                Ok(response) => {
                    let status = response.status().as_u16();
                    let retry_after = response
                        .headers()
                        .get(reqwest::header::RETRY_AFTER)
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| v.trim().parse::<u64>().ok())
                        .map(Duration::from_secs);
                    match response.bytes().await {
                        Ok(bytes) if (200..300).contains(&status) => {
                            return Ok((
                                RawResponse {
                                    body: bytes.to_vec(),
                                },
                                started.elapsed(),
                            ));
                        }
                        Ok(bytes) => (http_error(self.provider, status, &bytes), retry_after),
                        Err(err) => (transport_error(err), None),
                    }
                }
                Err(err) => (transport_error(err), None),
            };

            if attempt >= 2 || !err.is_retryable() {
                return Err(err);
            }

            // Un reintento, con la espera de Retry-After si cabe en el plazo o una corta con variación.
            let backoff = retry_after.unwrap_or_else(|| {
                use store::rand::RngExt;
                Duration::from_millis(store::rand::rng().random_range(200..600))
            });
            if Instant::now() + backoff >= deadline {
                return Err(err);
            }
            trc::event!(
                Ai(AiEvent::ApiError),
                SpanId = session_id,
                Model = self.name.clone(),
                Reason = err.message.clone(),
                Details = "retrying",
            );
            tokio::time::sleep(backoff).await;
        }
    }

    /// Llamada generativa completa: construye la petición del proveedor, la envía y la interpreta.
    pub async fn generate(
        &self,
        request: &AiRequest<'_>,
        deadline: Instant,
        session_id: u64,
    ) -> Result<AiResponse, AiError> {
        let body = match self.provider {
            Provider::OpenAi(profile) => super::openai::build_request(self, profile, request),
            Provider::Anthropic => super::anthropic::build_request(self, request),
            Provider::Gemini => super::gemini::build_request(self, request),
            Provider::Ollama => super::ollama::build_request(self, request),
            Provider::TypeSafe => {
                return Err(AiError::new(
                    AiErrorKind::Unsupported,
                    "Operation not supported by this model",
                ));
            }
        };
        let body = merge_extra(body, self.extra_parameters.as_ref());
        let (raw, elapsed) = self.post_json(&body, deadline, session_id).await?;
        let response = match self.provider {
            Provider::OpenAi(_) => super::openai::parse_response(&raw.body),
            Provider::Anthropic => super::anthropic::parse_response(&raw.body),
            Provider::Gemini => super::gemini::parse_response(&raw.body),
            Provider::Ollama => super::ollama::parse_response(&raw.body),
            Provider::TypeSafe => unreachable!(),
        }?;
        self.log_success(&response, elapsed, session_id);
        Ok(response)
    }

    pub fn log_success(&self, response: &AiResponse, elapsed: Duration, session_id: u64) {
        trc::event!(
            Ai(AiEvent::Request),
            SpanId = session_id,
            Model = self.name.clone(),
            Id = response.model.clone().unwrap_or_else(|| self.model.clone()),
            Elapsed = elapsed,
            InputTokens = response.input_tokens,
            OutputTokens = response.output_tokens,
            CachedTokens = response.cached_tokens,
            FinishReason = response.finish_reason.clone().unwrap_or_default(),
            Details = response.request_id.clone().unwrap_or_default(),
        );
        trc::event!(
            Ai(AiEvent::LlmResponse),
            SpanId = session_id,
            Model = self.name.clone(),
            Contents = truncate_chars(&response.text, 4096).to_string(),
        );
    }

    /// Registra un fallo de una llamada (evento `ai.api-error`).
    pub fn log_error(&self, err: &AiError, session_id: u64) {
        trc::event!(
            Ai(AiEvent::ApiError),
            SpanId = session_id,
            Model = self.name.clone(),
            Reason = err.message.clone(),
            Code = match err.kind {
                AiErrorKind::Http(s) | AiErrorKind::Auth(s) => s as u64,
                _ => 0,
            },
        );
    }

    /// Anota el resultado en un cortacircuitos (el del modelo o el del clasificador).
    pub fn record_breaker(
        &self,
        breaker: &Breaker,
        result: Result<(), &AiError>,
        threshold: u32,
        cooldown: Duration,
        session_id: u64,
    ) {
        match result {
            Ok(()) => breaker.success(),
            Err(err) if err.counts_as_failure() => {
                if breaker.failure(threshold, cooldown, err.opens_breaker_now()) {
                    trc::event!(
                        Ai(AiEvent::BreakerOpen),
                        SpanId = session_id,
                        Model = self.name.clone(),
                        Reason = err.message.clone(),
                        RetryAfter = cooldown,
                    );
                }
            }
            Err(_) => {}
        }
    }
}

fn transport_error(err: reqwest::Error) -> AiError {
    if err.is_timeout() {
        AiError::new(AiErrorKind::Timeout, "Request timed out")
    } else if err.is_connect() {
        AiError::new(AiErrorKind::Network, "Connection to the AI endpoint failed")
    } else if err.is_decode() || err.is_body() {
        AiError::new(
            AiErrorKind::Network,
            "Failed to read the AI endpoint response",
        )
    } else {
        AiError::new(
            AiErrorKind::Network,
            format!("Request failed: {}", without_url(&err)),
        )
    }
}

/// Mensaje de reqwest sin la URL (por si alguien pusiera un secreto en la consulta).
fn without_url(err: &reqwest::Error) -> String {
    let mut text = err.to_string();
    if let Some(url) = err.url() {
        text = text.replace(url.as_str(), "<url>");
    }
    text
}

/// Error HTTP del proveedor con su mensaje (si el cuerpo lo trae) y la clase de error.
pub fn http_error(provider: Provider, status: u16, body: &[u8]) -> AiError {
    let message = provider_error_message(body);
    let lower = message.to_ascii_lowercase();
    let billing = lower.contains("insufficient_quota")
        || lower.contains("quota")
        || lower.contains("billing")
        || lower.contains("credit balance")
        || lower.contains("spending limit");
    let kind = if matches!(status, 401..=403) || (billing && status != 503 && status != 529) {
        AiErrorKind::Auth(status)
    } else {
        AiErrorKind::Http(status)
    };
    let _ = provider;
    let message = if message.is_empty() {
        format!("HTTP {status}")
    } else {
        format!("HTTP {status}: {}", truncate_chars(&message, 300))
    };
    AiError::new(kind, message)
}

/// Extrae el mensaje de error de los formatos de OpenAI, Anthropic, Gemini, Ollama y TypeSafe.
fn provider_error_message(body: &[u8]) -> String {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(body) else {
        return String::from_utf8_lossy(body).chars().take(300).collect();
    };
    let error = value.get("error");
    if let Some(message) = error
        .and_then(|e| e.get("message"))
        .and_then(|m| m.as_str())
    {
        let code = error
            .and_then(|e| {
                e.get("code")
                    .or_else(|| e.get("type"))
                    .or_else(|| e.get("status"))
            })
            .and_then(|c| c.as_str());
        return match code {
            Some(code) => format!("{code}: {message}"),
            None => message.to_string(),
        };
    }
    if let Some(message) = error.and_then(|e| e.as_str()) {
        return message.to_string();
    }
    if let Some(detail) = value.get("detail") {
        return match detail {
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        };
    }
    String::new()
}

/// Mezcla `extraParameters` en el cuerpo (objetos en profundidad; el resto se sustituye).
pub fn merge_extra(
    mut body: serde_json::Value,
    extra: Option<&serde_json::Map<String, serde_json::Value>>,
) -> serde_json::Value {
    fn merge(target: &mut serde_json::Value, source: &serde_json::Value) {
        match (target, source) {
            (serde_json::Value::Object(t), serde_json::Value::Object(s)) => {
                for (key, value) in s {
                    match t.get_mut(key) {
                        Some(existing) if existing.is_object() && value.is_object() => {
                            merge(existing, value)
                        }
                        _ => {
                            t.insert(key.clone(), value.clone());
                        }
                    }
                }
            }
            (target, source) => *target = source.clone(),
        }
    }
    if let Some(extra) = extra {
        merge(&mut body, &serde_json::Value::Object(extra.clone()));
    }
    body
}

pub fn truncate_chars(text: &str, max: usize) -> &str {
    match text.char_indices().nth(max) {
        Some((idx, _)) => &text[..idx],
        None => text,
    }
}

pub fn parse_json(body: &[u8]) -> Result<serde_json::Value, AiError> {
    serde_json::from_slice(body).map_err(|_| AiError::invalid("Invalid JSON in AI response"))
}

/// Lee un entero de un objeto JSON siguiendo una ruta (`usage.prompt_tokens`…).
pub fn json_u64(value: &serde_json::Value, path: &[&str]) -> u64 {
    path.iter()
        .try_fold(value, |v, key| v.get(key))
        .and_then(|v| v.as_u64())
        .unwrap_or(0)
}

pub fn json_str(value: &serde_json::Value, path: &[&str]) -> Option<String> {
    path.iter()
        .try_fold(value, |v, key| v.get(key))
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

#[cfg(test)]
mod pruebas {
    use super::*;

    #[test]
    fn errores_http() {
        let e = http_error(
            Provider::Anthropic,
            401,
            br#"{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"}}"#,
        );
        assert!(e.opens_breaker_now());
        assert_eq!(
            e.message,
            "HTTP 401: authentication_error: invalid x-api-key"
        );
        let e = http_error(Provider::Ollama, 500, br#"{"error":"model not found"}"#);
        assert_eq!(e.kind, AiErrorKind::Http(500));
        assert!(e.is_retryable());
        let e = http_error(
            Provider::OpenAi(registry::schema::enums::AiOpenAiProfile::OpenAi),
            429,
            br#"{"error":{"message":"You exceeded your current quota","type":"insufficient_quota"}}"#,
        );
        assert!(e.opens_breaker_now());
        let e = http_error(Provider::Gemini, 503, b"<html>overloaded</html>");
        assert!(e.is_retryable() && !e.opens_breaker_now());
    }

    #[test]
    fn mezcla_de_parametros() {
        let body =
            serde_json::json!({"model": "m", "options": {"num_predict": 10, "temperature": 0.1}});
        let extra = serde_json::json!({"options": {"num_ctx": 8192}, "top_p": 0.9});
        let merged = merge_extra(body, extra.as_object());
        assert_eq!(
            merged,
            serde_json::json!({"model": "m", "top_p": 0.9, "options": {"num_predict": 10, "temperature": 0.1, "num_ctx": 8192}})
        );
    }

    #[test]
    fn cortacircuitos() {
        let b = Breaker::default();
        assert!(!b.failure(3, Duration::from_secs(60), false));
        assert!(!b.failure(3, Duration::from_secs(60), false));
        assert!(!b.is_open());
        assert!(b.failure(3, Duration::from_secs(60), false));
        assert!(b.is_open());
        let b = Breaker::default();
        assert!(b.failure(5, Duration::from_secs(60), true));
        assert!(b.is_open());
        let b = Breaker::default();
        b.failure(2, Duration::from_secs(60), false);
        b.success();
        assert!(!b.failure(2, Duration::from_secs(60), false));
    }

    #[test]
    fn ritmo_repartido() {
        let limiter = RateLimiter::new(2, Duration::from_secs(1));
        let deadline = Instant::now() + Duration::from_secs(10);
        let a = limiter.reserve(deadline).unwrap();
        let b = limiter.reserve(deadline).unwrap();
        assert!(b.duration_since(a) >= Duration::from_millis(499));
        let tight = Instant::now() + Duration::from_millis(100);
        assert!(limiter.reserve(tight).is_none());
    }

    #[test]
    fn truncado_por_caracteres() {
        assert_eq!(truncate_chars("ñandú", 3), "ñan");
        assert_eq!(truncate_chars("abc", 10), "abc");
    }
}
