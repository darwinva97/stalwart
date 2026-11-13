/*
 * SPDX-FileCopyrightText: 2026 bezenti
 *
 * SPDX-License-Identifier: AGPL-3.0-only
 */

//! Errores SCIM (RFC 7644 §3.12) y su correspondencia con los errores del registro y de `trc`.
//! Especificación: `bezenti/especificaciones/scim.md`, §11.

use http_proto::HttpResponse;
use hyper::StatusCode;
use jmap::api::ToRequestError;
use serde_json::{Value, json};
use trc::ScimEvent;

pub const SCIM_CONTENT_TYPE: &str = "application/scim+json; charset=utf-8";
pub const SCHEMA_ERROR: &str = "urn:ietf:params:scim:api:messages:2.0:Error";

/// Un error SCIM: estado HTTP, `scimType` (si el RFC define uno para el caso) y detalle legible.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScimError {
    pub status: u16,
    pub scim_type: Option<&'static str>,
    pub detail: String,
    /// Cabecera `Allow` (405).
    pub allow: Option<&'static str>,
    /// Cabecera `Retry-After` (429, 503).
    pub retry_after: Option<u64>,
    /// Cabecera `WWW-Authenticate` (401).
    pub challenge: bool,
}

pub type ScimResult<T> = Result<T, ScimError>;

impl ScimError {
    pub fn new(status: u16, scim_type: Option<&'static str>, detail: impl Into<String>) -> Self {
        ScimError {
            status,
            scim_type,
            detail: detail.into(),
            allow: None,
            retry_after: None,
            challenge: status == 401,
        }
    }

    pub fn invalid_syntax(detail: impl Into<String>) -> Self {
        Self::new(400, Some("invalidSyntax"), detail)
    }

    pub fn invalid_filter(detail: impl Into<String>) -> Self {
        Self::new(400, Some("invalidFilter"), detail)
    }

    pub fn invalid_path(detail: impl Into<String>) -> Self {
        Self::new(400, Some("invalidPath"), detail)
    }

    pub fn invalid_value(detail: impl Into<String>) -> Self {
        Self::new(400, Some("invalidValue"), detail)
    }

    pub fn too_many(detail: impl Into<String>) -> Self {
        Self::new(400, Some("tooMany"), detail)
    }

    pub fn mutability(detail: impl Into<String>) -> Self {
        Self::new(400, Some("mutability"), detail)
    }

    pub fn no_target(detail: impl Into<String>) -> Self {
        Self::new(400, Some("noTarget"), detail)
    }

    pub fn invalid_cursor() -> Self {
        Self::new(
            400,
            Some("invalidCursor"),
            "The supplied cursor was issued for a different query.",
        )
    }

    pub fn unauthorized(detail: impl Into<String>) -> Self {
        Self::new(401, None, detail)
    }

    pub fn forbidden(detail: impl Into<String>) -> Self {
        Self::new(403, None, detail)
    }

    pub fn not_found(detail: impl Into<String>) -> Self {
        Self::new(404, None, detail)
    }

    pub fn resource_not_found(id: &str) -> Self {
        Self::not_found(format!("Resource {id} not found"))
    }

    pub fn method_not_allowed(allow: &'static str) -> Self {
        let mut err = Self::new(405, None, "Method not allowed");
        err.allow = Some(allow);
        err
    }

    pub fn uniqueness(detail: impl Into<String>) -> Self {
        Self::new(409, Some("uniqueness"), detail)
    }

    pub fn conflict(detail: impl Into<String>) -> Self {
        Self::new(409, None, detail)
    }

    pub fn precondition_failed() -> Self {
        Self::new(
            412,
            None,
            "The resource has changed since the version given in If-Match",
        )
    }

    pub fn payload_too_large(limit: usize) -> Self {
        Self::new(413, None, format!("The request body exceeds {limit} bytes"))
    }

    pub fn not_implemented(detail: impl Into<String>) -> Self {
        Self::new(501, None, detail)
    }

    pub fn internal(detail: impl Into<String>) -> Self {
        Self::new(500, None, detail)
    }

    /// Evento `scim.*` que registra este error (doc: «Diagnosing failures»).
    pub fn event(&self) -> ScimEvent {
        match self.status {
            403 => ScimEvent::Forbidden,
            409 | 412 => ScimEvent::Conflict,
            _ => ScimEvent::Error,
        }
    }

    /// Documento de error SCIM.
    pub fn to_json(&self) -> Value {
        let mut doc = json!({
            "schemas": [SCHEMA_ERROR],
            "status": self.status.to_string(),
            "detail": self.detail,
        });
        if let Some(scim_type) = self.scim_type {
            doc["scimType"] = Value::from(scim_type);
        }
        doc
    }

    pub fn into_http_response(self) -> HttpResponse {
        let mut response = HttpResponse::new(
            StatusCode::from_u16(self.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        )
        .with_content_type(SCIM_CONTENT_TYPE)
        .with_no_store();
        if let Some(allow) = self.allow {
            response = response.with_header(hyper::header::ALLOW, allow);
        }
        if let Some(retry_after) = self.retry_after {
            response = response.with_header(hyper::header::RETRY_AFTER, retry_after.to_string());
        }
        if self.challenge {
            response =
                response.with_header(hyper::header::WWW_AUTHENTICATE, "Bearer realm=\"SCIM\"");
        }
        response.with_text_body(self.to_json().to_string())
    }

    /// Error de un `x:Account/set` del registro (`notCreated`, `notUpdated`, `notDestroyed`), ya en JSON.
    pub fn from_set_error(err: &Value) -> Self {
        let kind = err.get("type").and_then(|v| v.as_str()).unwrap_or_default();
        let description = err.get("description").and_then(|v| v.as_str());
        let detail = description.map(|d| d.to_string()).unwrap_or_else(|| {
            let properties = err
                .get("properties")
                .map(|p| p.to_string())
                .unwrap_or_default();
            format!("{kind} {properties}").trim().to_string()
        });
        match kind {
            "forbidden" | "overQuota" => Self::forbidden(detail),
            "primaryKeyViolation" => Self::uniqueness(if description.is_some() {
                detail
            } else {
                "An address or name in this request is already in use".to_string()
            }),
            "notFound" => Self::not_found(detail),
            "objectIsLinked" => Self::conflict(detail),
            _ => Self::invalid_value(detail),
        }
    }
}

impl From<trc::Error> for ScimError {
    fn from(err: trc::Error) -> Self {
        let event = err.as_ref().clone();
        if matches!(event, trc::EventType::Jmap(trc::JmapEvent::Forbidden))
            || matches!(
                event,
                trc::EventType::Security(trc::SecurityEvent::Unauthorized)
            )
        {
            let detail = err
                .value_as_str(trc::Key::Details)
                .unwrap_or("Forbidden")
                .to_string();
            return Self::forbidden(detail);
        }
        let request_error = err.to_request_error();
        let status = request_error.status;
        let mut scim = if status >= 500 {
            trc::error!(err.clone());
            ScimError::new(status, None, "Internal server error")
        } else {
            ScimError::new(status, None, request_error.detail.to_string())
        };
        scim.retry_after = request_error.retry_after;
        scim.challenge = status == 401;
        if status == 401 && scim.detail.is_empty() {
            scim.detail = "Authentication failed".to_string();
        }
        scim
    }
}

/// Respuesta SCIM correcta con su cuerpo JSON.
pub fn scim_json_response(status: StatusCode, body: &Value) -> HttpResponse {
    HttpResponse::new(status)
        .with_content_type(SCIM_CONTENT_TYPE)
        .with_no_store()
        .with_text_body(body.to_string())
}
