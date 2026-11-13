/*
 * SPDX-FileCopyrightText: 2026 bezenti
 *
 * SPDX-License-Identifier: AGPL-3.0-only
 */

//! IA de la edición bezenti (fase 4): modelos de lenguaje configurables (`x:AiModel`), clasificador
//! antispam con IA (`x:SpamAi`, la etapa vive en el crate `spam-filter`) y la función Sieve `llm_prompt`.
//!
//! Especificación: `bezenti/especificaciones/ia.md`. Escrito desde cero a partir de la documentación pública
//! de Stalwart, el esquema de la API y la documentación pública de cada proveedor.
//!
//! - `cliente`: transporte HTTP común (tiempo de espera, un reintento con espera, límites de concurrencia y de
//!   ritmo, cortacircuitos, telemetría `ai.*`). Las claves API nunca se registran.
//! - `openai`, `anthropic`, `gemini`, `ollama`, `typesafe`: el dialecto de cada proveedor.
//! - `clasificacion`: instrucciones, esquema JSON del veredicto y lectura de la respuesta (con `logprobs`).
//! - `sieve`: la función `llm_prompt`.

pub mod anthropic;
pub mod clasificacion;
pub mod cliente;
pub mod gemini;
pub mod ollama;
pub mod openai;
pub mod sieve;
pub mod typesafe;

use crate::expr::if_block::{BootstrapExprExt, IfBlock};
use ahash::{AHashMap, AHashSet};
use cliente::{Breaker, RateLimiter};
use registry::schema::{
    enums::{AiOpenAiProfile, AiReasoning, SpamAiMode},
    prelude::ObjectType,
    structs::{self, AiModel, SpamAi, SpamAiLearn},
};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use std::{sync::Arc, time::Duration};
use store::registry::bootstrap::Bootstrap;
use tokio::sync::Semaphore;
use types::id::Id;
use utils::glob::GlobMap;

use crate::config::mailstore::spamfilter::SpamFilterAction;

/// Configuración de IA cargada del registro (parte de `Core`).
#[derive(Default)]
pub struct AiConfig {
    /// Modelos por identificador del registro (los usa `x:SpamAi`).
    pub models: AHashMap<Id, Arc<AiModelRuntime>>,
    /// Modelos por nombre (los usa `llm_prompt`).
    pub models_by_name: AHashMap<String, Arc<AiModelRuntime>>,
    /// Clasificador antispam con IA, si está activado.
    pub spam: Option<Arc<SpamAiConfig>>,
}

/// Proveedor (variante de `x:AiModel`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    OpenAi(AiOpenAiProfile),
    Anthropic,
    Gemini,
    Ollama,
    TypeSafe,
}

/// Un modelo listo para usar: cliente HTTP, límites y estado del cortacircuitos.
pub struct AiModelRuntime {
    pub id: Id,
    pub name: String,
    pub model: String,
    pub provider: Provider,
    pub url: String,
    pub client: reqwest::Client,
    pub max_output_tokens: u64,
    pub temperature: Option<f64>,
    pub reasoning: AiReasoning,
    pub logprobs: bool,
    pub keep_alive: Option<Duration>,
    pub extra_parameters: Option<serde_json::Map<String, serde_json::Value>>,
    pub timeout: Duration,
    pub concurrency: Semaphore,
    pub rate_limiter: Option<RateLimiter>,
    /// Cortacircuitos propio del modelo (llm_prompt): 5 fallos seguidos, 60 s (documentación pública).
    pub breaker: Breaker,
}

impl std::fmt::Debug for AiModelRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Nunca se imprimen las cabeceras (llevan la clave API).
        f.debug_struct("AiModelRuntime")
            .field("name", &self.name)
            .field("model", &self.model)
            .field("provider", &self.provider)
            .field("url", &self.url)
            .finish()
    }
}

impl AiModelRuntime {
    pub fn is_generative(&self) -> bool {
        !matches!(self.provider, Provider::TypeSafe)
    }
}

/// `x:SpamAi` compilado.
pub struct SpamAiConfig {
    pub shadow: bool,
    pub condition: IfBlock,
    pub tiers: Vec<SpamAiTier>,
    /// Instrucciones completas (`instructions` + categorías + señales + formato de respuesta).
    pub system_prompt: String,
    pub instructions: String,
    pub categories: Vec<SpamAiCategory>,
    pub signals: Vec<SpamAiSignal>,
    pub explanation: bool,
    pub bucket_high: f32,
    pub bucket_medium: f32,
    pub uncertain_below: f32,
    pub verbal_high: f32,
    pub verbal_medium: f32,
    pub verbal_low: f32,
    pub add_header: bool,
    pub max_input_chars: usize,
    pub max_links: usize,
    pub max_attachments: usize,
    pub trim_quotes: bool,
    pub include_auth: bool,
    pub include_links: bool,
    pub include_attachments: bool,
    pub deadline: Duration,
    pub breaker_threshold: u32,
    pub breaker_cooldown: Duration,
    /// `(minProbability, sampleRate)` si `learn` está activado.
    pub learn: Option<(f32, f32)>,
    /// Esquema JSON del veredicto (salida estructurada).
    pub schema: serde_json::Value,
}

pub struct SpamAiTier {
    pub model: Arc<AiModelRuntime>,
    pub escalate_below: f32,
    pub escalate_on: AHashSet<String>,
    /// Cortacircuitos del clasificador para este modelo (`breakerThreshold`/`breakerCooldown`).
    pub breaker: Breaker,
}

pub struct SpamAiCategory {
    pub name: String,
    /// Nombre en mayúsculas para las etiquetas (`AI_<CATEGORÍA>_…`).
    pub tag: String,
    pub description: String,
    pub includes: Vec<String>,
}

pub struct SpamAiSignal {
    pub name: String,
    pub tag: String,
    pub question: String,
}

/// Límite de salida propio del clasificador (documentación pública: «the spam classifier uses its own smaller
/// limit»). Valor bezenti: lo justo para el JSON del veredicto, más una explicación corta si se pide.
pub const SPAM_MAX_OUTPUT_TOKENS: u64 = 400;
pub const SPAM_MAX_OUTPUT_TOKENS_EXPLAINED: u64 = 700;

/// Puntuaciones por defecto de las etiquetas de IA cuando ningún `x:SpamTag` las define.
/// Las marcadas [doc] son las de la documentación pública; el resto es decisión bezenti (ver ia.md, §5.6).
pub const DEFAULT_AI_SCORES: &[(&str, f32)] = &[
    ("AI_PHISHING_HIGH", 7.0),    // [doc]
    ("AI_BULK_SPAM_HIGH", 5.0),   // [doc]
    ("AI_LEGITIMATE_HIGH", -3.0), // [doc]
    ("AI_IMPERSONATION", 3.0),    // [doc]
    ("AI_UNCERTAIN", 0.0),        // [doc]
    ("AI_ERROR", 0.0),            // [doc]
    ("AI_PHISHING_MEDIUM", 4.0),
    ("AI_PHISHING_LOW", 1.5),
    ("AI_MALWARE_HIGH", 7.0),
    ("AI_MALWARE_MEDIUM", 4.0),
    ("AI_MALWARE_LOW", 1.5),
    ("AI_BULK_SPAM_MEDIUM", 3.0),
    ("AI_BULK_SPAM_LOW", 1.0),
    ("AI_UNSOLICITED_SALES_HIGH", 2.5),
    ("AI_UNSOLICITED_SALES_MEDIUM", 1.5),
    ("AI_UNSOLICITED_SALES_LOW", 0.5),
    ("AI_LEGITIMATE_MEDIUM", -1.5),
    ("AI_LEGITIMATE_LOW", -0.5),
    ("AI_TRANSACTIONAL_HIGH", -2.0),
    ("AI_TRANSACTIONAL_MEDIUM", -1.0),
    ("AI_NEWSLETTER_HIGH", -1.0),
    ("AI_NEWSLETTER_MEDIUM", -0.5),
    ("AI_CREDENTIAL_REQUEST", 1.5),
    ("AI_REQUESTED", -1.0),
];

/// Nombre de categoría o señal → parte de etiqueta (`bulk_spam` → `BULK_SPAM`).
pub fn tag_part(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect()
}

impl AiConfig {
    pub async fn parse(bp: &mut Bootstrap) -> Self {
        let mut config = AiConfig::default();

        for obj in bp.list_infallible::<AiModel>().await {
            match AiModelRuntime::build(obj.id.id(), obj.object).await {
                Ok(model) => {
                    let model = Arc::new(model);
                    config
                        .models_by_name
                        .insert(model.name.clone(), model.clone());
                    config.models.insert(model.id, model);
                }
                Err(err) => bp.build_error(obj.id, err),
            }
        }

        let object_id = ObjectType::SpamAi.singleton();
        if let SpamAi::Enable(properties) = bp.setting_infallible::<SpamAi>().await {
            let condition = bp.compile_expr(object_id, &properties.ctx_condition());
            let mut tiers = Vec::with_capacity(properties.tiers.len());
            for tier in properties.tiers.iter() {
                if let Some(model) = config.models.get(&tier.model_id) {
                    tiers.push(SpamAiTier {
                        model: model.clone(),
                        escalate_below: tier.escalate_below.into_inner() as f32,
                        escalate_on: tier
                            .escalate_on
                            .iter()
                            .map(|c| c.to_ascii_lowercase())
                            .collect(),
                        breaker: Breaker::default(),
                    });
                } else {
                    bp.build_warning(
                        object_id,
                        format!(
                            "AI model {} referenced by a tier is not available",
                            tier.model_id
                        ),
                    );
                }
            }

            let categories = properties
                .categories
                .iter()
                .map(|c| SpamAiCategory {
                    name: c.name.clone(),
                    tag: tag_part(&c.name),
                    description: c.description.clone(),
                    includes: c.includes.iter().cloned().collect(),
                })
                .collect::<Vec<_>>();
            let signals = properties
                .signals
                .iter()
                .map(|s| SpamAiSignal {
                    name: s.name.clone(),
                    tag: tag_part(&s.name),
                    question: s.question.clone(),
                })
                .collect::<Vec<_>>();
            let schema =
                clasificacion::verdict_schema(&categories, &signals, properties.explanation);
            let system_prompt = clasificacion::system_prompt(
                &properties.instructions,
                &categories,
                &signals,
                properties.explanation,
            );

            config.spam = Some(Arc::new(SpamAiConfig {
                shadow: properties.mode == SpamAiMode::Shadow,
                condition,
                tiers,
                system_prompt,
                instructions: properties.instructions.clone(),
                categories,
                signals,
                explanation: properties.explanation,
                bucket_high: properties.bucket_high.into_inner() as f32,
                bucket_medium: properties.bucket_medium.into_inner() as f32,
                uncertain_below: properties.uncertain_below.into_inner() as f32,
                verbal_high: properties.verbal_high.into_inner() as f32,
                verbal_medium: properties.verbal_medium.into_inner() as f32,
                verbal_low: properties.verbal_low.into_inner() as f32,
                add_header: properties.add_header,
                max_input_chars: properties.max_input_chars as usize,
                max_links: properties.max_links as usize,
                max_attachments: properties.max_attachments as usize,
                trim_quotes: properties.trim_quotes,
                include_auth: properties.include_auth,
                include_links: properties.include_links,
                include_attachments: properties.include_attachments,
                deadline: properties.deadline.into_inner(),
                breaker_threshold: properties.breaker_threshold.max(1) as u32,
                breaker_cooldown: properties.breaker_cooldown.into_inner(),
                learn: match &properties.learn {
                    SpamAiLearn::Enable(learn) => Some((
                        learn.min_probability.into_inner() as f32,
                        learn.sample_rate.into_inner() as f32,
                    )),
                    SpamAiLearn::Disable => None,
                },
                schema,
            }));
        }

        config
    }

    /// Modelo por nombre; con un único modelo configurado, el nombre vacío lo elige (documentación pública).
    pub fn model_by_name(&self, name: &str) -> Option<&Arc<AiModelRuntime>> {
        if name.is_empty() && self.models.len() == 1 {
            self.models.values().next()
        } else {
            self.models_by_name.get(name)
        }
    }

    /// Añade las puntuaciones por defecto de las etiquetas de IA que nadie ha definido con `x:SpamTag`.
    pub fn apply_default_scores(&self, scores: &mut GlobMap<SpamFilterAction<f32>>) {
        if self.spam.is_none() {
            return;
        }
        for (tag, score) in DEFAULT_AI_SCORES {
            if scores.get(tag).is_none() {
                scores.insert_entry(tag.to_string(), SpamFilterAction::Allow(*score));
            }
        }
    }
}

impl AiModelRuntime {
    async fn build(id: Id, model: AiModel) -> Result<Self, String> {
        struct Common {
            name: String,
            model: String,
            url: Option<String>,
            api_key: structs::SecretKeyOptional,
            allow_invalid_certs: bool,
            http_headers: Vec<(String, String)>,
            max_output_tokens: u64,
            temperature: Option<f64>,
            reasoning: AiReasoning,
            logprobs: bool,
            keep_alive: Option<Duration>,
            extra_parameters: Option<String>,
            timeout: Duration,
            max_concurrent_requests: u64,
            rate_limit: Option<structs::Rate>,
        }
        let (provider, c) = match model {
            AiModel::OpenAi(m) => (
                Provider::OpenAi(m.profile),
                Common {
                    name: m.name,
                    model: m.model,
                    url: m.url,
                    api_key: m.api_key,
                    allow_invalid_certs: m.allow_invalid_certs,
                    http_headers: m.http_headers.into_iter().collect(),
                    max_output_tokens: m.max_output_tokens,
                    temperature: m.temperature.map(|t| t.into_inner()),
                    reasoning: m.reasoning,
                    logprobs: m.logprobs,
                    keep_alive: None,
                    extra_parameters: m.extra_parameters,
                    timeout: m.timeout.into_inner(),
                    max_concurrent_requests: m.max_concurrent_requests,
                    rate_limit: m.rate_limit,
                },
            ),
            AiModel::Anthropic(m) => (
                Provider::Anthropic,
                Common {
                    name: m.name,
                    model: m.model,
                    url: m.url,
                    api_key: m.api_key,
                    allow_invalid_certs: m.allow_invalid_certs,
                    http_headers: m.http_headers.into_iter().collect(),
                    max_output_tokens: m.max_output_tokens,
                    temperature: m.temperature.map(|t| t.into_inner()),
                    reasoning: m.reasoning,
                    logprobs: false,
                    keep_alive: None,
                    extra_parameters: m.extra_parameters,
                    timeout: m.timeout.into_inner(),
                    max_concurrent_requests: m.max_concurrent_requests,
                    rate_limit: m.rate_limit,
                },
            ),
            AiModel::Gemini(m) => (
                Provider::Gemini,
                Common {
                    name: m.name,
                    model: m.model,
                    url: m.url,
                    api_key: m.api_key,
                    allow_invalid_certs: m.allow_invalid_certs,
                    http_headers: m.http_headers.into_iter().collect(),
                    max_output_tokens: m.max_output_tokens,
                    temperature: m.temperature.map(|t| t.into_inner()),
                    reasoning: m.reasoning,
                    logprobs: m.logprobs,
                    keep_alive: None,
                    extra_parameters: m.extra_parameters,
                    timeout: m.timeout.into_inner(),
                    max_concurrent_requests: m.max_concurrent_requests,
                    rate_limit: m.rate_limit,
                },
            ),
            AiModel::Ollama(m) => (
                Provider::Ollama,
                Common {
                    name: m.name,
                    model: m.model,
                    url: m.url,
                    api_key: m.api_key,
                    allow_invalid_certs: m.allow_invalid_certs,
                    http_headers: m.http_headers.into_iter().collect(),
                    max_output_tokens: m.max_output_tokens,
                    temperature: m.temperature.map(|t| t.into_inner()),
                    reasoning: m.reasoning,
                    logprobs: m.logprobs,
                    keep_alive: m.keep_alive.map(|d| d.into_inner()),
                    extra_parameters: m.extra_parameters,
                    timeout: m.timeout.into_inner(),
                    max_concurrent_requests: m.max_concurrent_requests,
                    rate_limit: m.rate_limit,
                },
            ),
            AiModel::TypeSafe(m) => (
                Provider::TypeSafe,
                Common {
                    name: m.name,
                    model: m.model,
                    url: m.url,
                    api_key: m.api_key,
                    allow_invalid_certs: m.allow_invalid_certs,
                    http_headers: m.http_headers.into_iter().collect(),
                    max_output_tokens: 0,
                    temperature: None,
                    reasoning: AiReasoning::Default,
                    logprobs: false,
                    keep_alive: None,
                    extra_parameters: None,
                    timeout: m.timeout.into_inner(),
                    max_concurrent_requests: m.max_concurrent_requests,
                    rate_limit: m.rate_limit,
                },
            ),
        };

        // URL base: la del modelo o la del proveedor (documentación pública, tabla de proveedores).
        let url = c
            .url
            .filter(|u| !u.trim().is_empty())
            .unwrap_or_else(|| default_url(provider).to_string());
        let url = endpoint_url(provider, url.trim(), &c.model).ok_or("Invalid AI model URL")?;

        // Clave API (secreto del registro: valor, variable de entorno o archivo). Nunca se registra.
        let api_key = c
            .api_key
            .secret()
            .await
            .map_err(|_| "Unable to obtain AI model API key".to_string())?;

        let extra_parameters = match c.extra_parameters.as_deref().map(str::trim) {
            Some(text) if !text.is_empty() => match serde_json::from_str::<serde_json::Value>(text)
            {
                Ok(serde_json::Value::Object(map)) => Some(map),
                _ => return Err("Extra parameters must be a JSON object".into()),
            },
            _ => None,
        };

        let mut headers = HeaderMap::new();
        for (name, value) in &c.http_headers {
            let name = HeaderName::try_from(name.as_str())
                .map_err(|_| format!("Invalid HTTP header name {name:?}"))?;
            let value = HeaderValue::try_from(value.as_str())
                .map_err(|_| format!("Invalid value for HTTP header {name}"))?;
            headers.insert(name, value);
        }
        if let Some(key) = api_key.as_deref().filter(|k| !k.is_empty()) {
            let (name, value) = match provider {
                Provider::OpenAi(AiOpenAiProfile::Azure) => ("api-key", key.to_string()),
                Provider::Anthropic => ("x-api-key", key.to_string()),
                Provider::Gemini => ("x-goog-api-key", key.to_string()),
                _ => ("authorization", format!("Bearer {key}")),
            };
            let mut value = HeaderValue::try_from(value)
                .map_err(|_| "Unable to obtain AI model API key".to_string())?;
            value.set_sensitive(true);
            headers.insert(HeaderName::from_static(name), value);
        }
        if provider == Provider::Anthropic {
            headers.insert(
                HeaderName::from_static("anthropic-version"),
                HeaderValue::from_static(anthropic::API_VERSION),
            );
        }

        let client = utils::http::http_client_builder(c.allow_invalid_certs)
            .connect_timeout(Duration::from_secs(10).min(c.timeout.max(Duration::from_secs(1))))
            .user_agent(crate::USER_AGENT)
            .default_headers(headers)
            .build()
            .map_err(|err| format!("Failed to build HTTP client: {err}"))?;

        Ok(AiModelRuntime {
            id,
            name: c.name,
            model: c.model,
            provider,
            url,
            client,
            max_output_tokens: c.max_output_tokens.max(1),
            temperature: c.temperature,
            reasoning: c.reasoning,
            logprobs: c.logprobs,
            keep_alive: c.keep_alive,
            extra_parameters,
            timeout: if c.timeout.is_zero() {
                Duration::from_secs(60)
            } else {
                c.timeout
            },
            concurrency: Semaphore::new(c.max_concurrent_requests.clamp(1, 4096) as usize),
            rate_limiter: c
                .rate_limit
                .filter(|r| r.count > 0 && !r.period.into_inner().is_zero())
                .map(|r| RateLimiter::new(r.count, r.period.into_inner())),
            breaker: Breaker::default(),
        })
    }
}

pub fn default_url(provider: Provider) -> &'static str {
    match provider {
        Provider::OpenAi(_) => "https://api.openai.com/v1",
        Provider::Anthropic => "https://api.anthropic.com",
        Provider::Gemini => "https://generativelanguage.googleapis.com/v1beta",
        Provider::Ollama => "http://localhost:11434",
        Provider::TypeSafe => "https://api.typesafe.ai",
    }
}

/// URL completa del punto de la API: se añade la ruta salvo que la URL ya termine en ella (documentación
/// pública). La consulta (`?api-version=…` de Azure) se conserva al final.
pub fn endpoint_url(provider: Provider, base: &str, model: &str) -> Option<String> {
    let parsed = reqwest::Url::parse(base).ok()?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        return None;
    }
    let (path_part, query) = match base.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (base, None),
    };
    let path_part = path_part.trim_end_matches('/');
    let full = match provider {
        Provider::OpenAi(_) => {
            if path_part.ends_with("/chat/completions") {
                path_part.to_string()
            } else {
                format!("{path_part}/chat/completions")
            }
        }
        Provider::Anthropic => {
            if path_part.ends_with("/v1/messages") {
                path_part.to_string()
            } else if path_part.ends_with("/v1") {
                format!("{path_part}/messages")
            } else {
                format!("{path_part}/v1/messages")
            }
        }
        Provider::Gemini => {
            if path_part.contains(":generateContent") {
                path_part.to_string()
            } else {
                let model = model.trim_start_matches("models/");
                format!("{path_part}/models/{model}:generateContent")
            }
        }
        Provider::Ollama => {
            if path_part.ends_with("/api/chat") {
                path_part.to_string()
            } else {
                format!("{path_part}/api/chat")
            }
        }
        Provider::TypeSafe => {
            if path_part.ends_with("/v1/systemone") {
                path_part.to_string()
            } else {
                format!("{path_part}/v1/systemone")
            }
        }
    };
    Some(match query {
        Some(q) => format!("{full}?{q}"),
        None => full,
    })
}

#[cfg(test)]
mod pruebas {
    use super::*;

    #[test]
    fn urls_de_cada_proveedor() {
        let p = Provider::OpenAi(AiOpenAiProfile::OpenAi);
        assert_eq!(
            endpoint_url(p, "https://api.openai.com/v1", "m").unwrap(),
            "https://api.openai.com/v1/chat/completions"
        );
        assert_eq!(
            endpoint_url(p, "http://10.0.0.12:8000/v1/chat/completions", "m").unwrap(),
            "http://10.0.0.12:8000/v1/chat/completions"
        );
        assert_eq!(
            endpoint_url(
                Provider::OpenAi(AiOpenAiProfile::Azure),
                "https://x.openai.azure.com/openai/deployments/d?api-version=2024-10-21",
                "m"
            )
            .unwrap(),
            "https://x.openai.azure.com/openai/deployments/d/chat/completions?api-version=2024-10-21"
        );
        assert_eq!(
            endpoint_url(Provider::Anthropic, "https://api.anthropic.com", "m").unwrap(),
            "https://api.anthropic.com/v1/messages"
        );
        assert_eq!(
            endpoint_url(Provider::Anthropic, "http://proxy:9/v1/", "m").unwrap(),
            "http://proxy:9/v1/messages"
        );
        assert_eq!(
            endpoint_url(
                Provider::Gemini,
                "https://generativelanguage.googleapis.com/v1beta",
                "models/gemini-x"
            )
            .unwrap(),
            "https://generativelanguage.googleapis.com/v1beta/models/gemini-x:generateContent"
        );
        assert_eq!(
            endpoint_url(Provider::Ollama, "http://localhost:11434/", "m").unwrap(),
            "http://localhost:11434/api/chat"
        );
        assert_eq!(
            endpoint_url(Provider::TypeSafe, "https://api.typesafe.ai", "m").unwrap(),
            "https://api.typesafe.ai/v1/systemone"
        );
        assert!(endpoint_url(p, "no es una url", "m").is_none());
        assert!(endpoint_url(p, "ftp://x/v1", "m").is_none());
    }

    #[test]
    fn partes_de_etiqueta() {
        assert_eq!(tag_part("bulk_spam"), "BULK_SPAM");
        assert_eq!(tag_part("credential-request"), "CREDENTIAL_REQUEST");
    }
}
