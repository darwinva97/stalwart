/*
 * SPDX-FileCopyrightText: 2026 bezenti
 *
 * SPDX-License-Identifier: AGPL-3.0-only
 */

//! Logo de marca por dominio o inquilino (edición bezenti, fase 2).
//!
//! Según la documentación pública (<https://stalw.art/docs/management/webui/branding/>), `GET /logo` sirve el
//! logo del **host de la petición**, de lo más concreto a lo más general: `logo` del dominio, `logo` del
//! inquilino dueño del dominio (y, en la edición bezenti, el de sus ancestros revendedores) y el integrado. El
//! «logo general» de Stalwart es parte de `x:Enterprise` (licencias) y no se implementa: sin logo propio se
//! responde que no hay (la interfaz web usa el suyo).
//!
//! Valores aceptados: URL https (http solo en dominios sin inquilino), base64 o URI `data:`; formatos PNG, JPEG,
//! GIF, WebP, ICO o SVG; máximo 1 MiB. Las URL se descargan (15 s, máximo 5 redirecciones) con la política de red
//! `PublicOnly` si el dominio es de un inquilino. El resultado se guarda en caché por host.

use crate::{LogoCache, Server, USER_AGENT, manager::application::Resource};
use bytes::Bytes;
use encodify::base64::LENIENT;
use registry::schema::structs::Domain;
use reqwest::{Method, StatusCode};
use std::time::{Duration, Instant};
use trc::AddContext;
use types::id::Id;
use utils::{
    HttpLimitResponse,
    http::http_client_builder,
    outbound::{OutboundClient, OutboundPolicy, Redirects},
};

/// Tamaño máximo de un logo.
pub const MAX_LOGO_SIZE: usize = 1024 * 1024;
const LOGO_FETCH_TIMEOUT: Duration = Duration::from_secs(15);
const LOGO_MAX_REDIRECTS: usize = 5;
/// Cuánto se guarda un logo encontrado (los cambios de dominio o inquilino lo invalidan antes).
const LOGO_TTL: Duration = Duration::from_secs(3600);
/// Cuánto se guarda que un host no tiene logo.
const LOGO_NEGATIVE_TTL: Duration = Duration::from_secs(300);

/// Datos que guarda `LogoCache` en la edición bezenti.
#[derive(Clone)]
pub struct CachedLogo {
    pub resource: Option<Resource<Bytes>>,
    pub expires: Instant,
}

impl Server {
    /// Logo del host `host` (un nombre de dominio o un subdominio de un dominio alojado).
    pub async fn bezenti_logo(&self, host: &str) -> trc::Result<Option<Resource<Bytes>>> {
        let host = host
            .rsplit_once(':')
            .filter(|(_, port)| port.bytes().all(|b| b.is_ascii_digit()))
            .map_or(host, |(host, _)| host)
            .trim_end_matches('.')
            .to_ascii_lowercase();
        if host.is_empty() {
            return Ok(None);
        }

        // Caché por host
        if let Some(cached) = self.inner.data.logos.lock().get(host.as_str())
            && cached.logo.expires > Instant::now()
        {
            return Ok(cached.logo.resource.clone());
        }

        // Dominio alojado: el propio host o el dominio más cercano por encima (mail.ejemplo.com → ejemplo.com).
        let mut domain = None;
        let mut candidate = host.as_str();
        loop {
            if let Some(found) = self.domain(candidate).await? {
                domain = Some(found);
                break;
            }
            match candidate.split_once('.') {
                Some((_, rest)) if rest.contains('.') => candidate = rest,
                _ => break,
            }
        }

        let (domain_id, resource) = match domain {
            Some(domain) => (domain.id, self.domain_logo(domain.id).await?),
            None => (u32::MAX, None),
        };

        self.inner.data.logos.lock().insert(
            host.into_boxed_str(),
            LogoCache {
                domain_id,
                logo: CachedLogo {
                    expires: Instant::now()
                        + if resource.is_some() {
                            LOGO_TTL
                        } else {
                            LOGO_NEGATIVE_TTL
                        },
                    resource: resource.clone(),
                },
            },
        );

        Ok(resource)
    }

    /// Logo de un dominio: el suyo o el de su inquilino (y ancestros).
    async fn domain_logo(&self, domain_id: u32) -> trc::Result<Option<Resource<Bytes>>> {
        let Some(domain) = self
            .registry()
            .object::<Domain>(Id::from(domain_id))
            .await
            .caused_by(trc::location!())?
        else {
            return Ok(None);
        };
        let tenant_id = domain.member_tenant_id.map(|id| id.document_id());
        // http solo para dominios del operador (documentación pública).
        let policy = if tenant_id.is_some() {
            OutboundPolicy::PublicOnly
        } else {
            OutboundPolicy::Trusted
        };

        let mut sources = Vec::new();
        if let Some(logo) = domain.logo.filter(|l| !l.trim().is_empty()) {
            sources.push(logo);
        }
        if let Some(tenant_id) = tenant_id {
            for tenant in self.tenant_ancestors(tenant_id).await? {
                if let Some(logo) = tenant.logo.as_ref().filter(|l| !l.trim().is_empty()) {
                    sources.push(logo.to_string());
                }
            }
        }

        for source in sources {
            match load_logo(&source, policy).await {
                Ok(resource) => return Ok(Some(resource)),
                Err(reason) => {
                    trc::event!(
                        Resource(trc::ResourceEvent::Error),
                        Id = domain_id,
                        Reason = reason,
                        Details = "Failed to load the logo of a domain or tenant",
                    );
                }
            }
        }

        Ok(None)
    }
}

/// Carga un logo desde su valor (URL, URI `data:` o base64) y comprueba su formato.
pub async fn load_logo(value: &str, policy: OutboundPolicy) -> Result<Resource<Bytes>, String> {
    let value = value.trim();
    let (declared_type, bytes) = if let Some(data) = value.strip_prefix("data:") {
        let (meta, payload) = data.split_once(',').ok_or("invalid data URI")?;
        let bytes = if meta.ends_with(";base64") {
            LENIENT
                .decode(payload.trim().as_bytes())
                .map_err(|_| "invalid base64 in data URI")?
        } else {
            percent_encoding::percent_decode_str(payload).collect::<Vec<u8>>()
        };
        (meta.split(';').next().map(|m| m.to_string()), bytes)
    } else if value.starts_with("https://") || value.starts_with("http://") {
        (None, fetch_logo(value, policy).await?)
    } else {
        (
            None,
            LENIENT
                .decode(value.as_bytes())
                .map_err(|_| "the logo is neither a URL nor valid base64")?,
        )
    };

    if bytes.is_empty() {
        return Err("empty logo".into());
    }
    if bytes.len() > MAX_LOGO_SIZE {
        return Err(format!("logo larger than {MAX_LOGO_SIZE} bytes"));
    }
    let content_type = detect_image_type(&bytes).ok_or_else(|| {
        format!(
            "unsupported logo format{}",
            declared_type.map(|t| format!(" ({t})")).unwrap_or_default()
        )
    })?;
    Ok(Resource::new(content_type, Bytes::from(bytes)))
}

async fn fetch_logo(url: &str, policy: OutboundPolicy) -> Result<Vec<u8>, String> {
    let response = OutboundClient::new(
        http_client_builder(false)
            .user_agent(USER_AGENT)
            .timeout(LOGO_FETCH_TIMEOUT),
        policy,
        Redirects::Limited(LOGO_MAX_REDIRECTS),
    )
    .request(Method::GET, url)
    .map_err(|err| format!("URL not allowed: {}", err.as_str()))?
    .send()
    .await
    .map_err(|err| format!("failed to fetch logo: {err}"))?;
    if response.status() != StatusCode::OK {
        return Err(format!("logo URL returned {}", response.status()));
    }
    response
        .bytes_with_limit(MAX_LOGO_SIZE)
        .await
        .map_err(|err| format!("failed to read logo: {err}"))?
        .ok_or_else(|| format!("logo larger than {MAX_LOGO_SIZE} bytes"))
}

/// Tipo MIME por los primeros bytes (solo los formatos de la documentación).
pub fn detect_image_type(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() > 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else if bytes.starts_with(&[0, 0, 1, 0]) {
        Some("image/x-icon")
    } else {
        // SVG: texto XML con un elemento <svg (se sirve con una CSP que no deja ejecutar scripts).
        let head = &bytes[..bytes.len().min(1024)];
        let text = std::str::from_utf8(head)
            .ok()?
            .trim_start_matches('\u{feff}')
            .trim_start();
        (text.starts_with('<') && text.to_ascii_lowercase().contains("<svg"))
            .then_some("image/svg+xml")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detecta_formatos() {
        assert_eq!(
            detect_image_type(b"\x89PNG\r\n\x1a\n...."),
            Some("image/png")
        );
        assert_eq!(
            detect_image_type(&[0xFF, 0xD8, 0xFF, 0xE0]),
            Some("image/jpeg")
        );
        assert_eq!(detect_image_type(b"GIF89a...."), Some("image/gif"));
        assert_eq!(
            detect_image_type(b"<?xml version=\"1.0\"?><svg xmlns=\"x\"></svg>"),
            Some("image/svg+xml")
        );
        assert_eq!(detect_image_type(b"<html><body>no</body></html>"), None);
        assert_eq!(detect_image_type(b"hola"), None);
    }

    #[tokio::test]
    async fn carga_data_uri_y_base64() {
        let png = b"\x89PNG\r\n\x1a\nresto";
        let b64 = encodify::base64::STANDARD.encode(png);
        let r = load_logo(
            &format!("data:image/png;base64,{b64}"),
            OutboundPolicy::PublicOnly,
        )
        .await
        .unwrap();
        assert_eq!(r.content_type, "image/png");
        assert_eq!(&r.contents[..], png);
        let r = load_logo(&b64, OutboundPolicy::PublicOnly).await.unwrap();
        assert_eq!(&r.contents[..], png);
        assert!(
            load_logo("bm8gZXMgdW5hIGltYWdlbg==", OutboundPolicy::PublicOnly)
                .await
                .is_err()
        );
        // Una URL de red privada no se descarga para un inquilino.
        assert!(
            load_logo("https://127.0.0.1/logo.png", OutboundPolicy::PublicOnly)
                .await
                .is_err()
        );
    }
}
