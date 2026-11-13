/*
 * SPDX-FileCopyrightText: 2026 bezenti
 *
 * SPDX-License-Identifier: AGPL-3.0-only
 */

//! Edición bezenti, fase 7: proveedor de servicio **SCIM 2.0** (RFC 7643, RFC 7644, RFC 9865) en `/scim/v2`.
//!
//! Aprovisionamiento de usuarios y grupos desde Entra ID, Okta, Keycloak (con extensión), Google… Escrito desde
//! cero a partir de la documentación pública de Stalwart, los RFC y la documentación pública de los clientes:
//! ver `bezenti/especificaciones/scim.md`. Nada que ver con los crates `scim`/`scim-proto` de la rama limpia
//! (eran SEL y están vacíos). Solo lo enlaza `http` con su feature `bezenti`.
//!
//! - Autenticación: la hace `http` (portador; Basic → 401) antes de llamar a [`ScimServidor::handle_scim_request`];
//!   los endpoints de descubrimiento no la piden.
//! - Lecturas filtradas por el ámbito (dominios abiertos a SCIM e inquilino del token); escrituras por el
//!   `x:Account/set` del registro, con todas sus reglas (ver `contexto.rs`).

#![warn(clippy::large_futures)]
// Con Redis compilado, comprobar que los futuros son `Send` pasa del límite por defecto (igual que en `jmap`).
#![recursion_limit = "256"]

pub mod bulk;
pub mod consulta;
pub mod contexto;
pub mod error;
pub mod esquema;
pub mod filtro;
pub mod patch;
pub mod recursos;

use crate::{
    consulta::{Parametros, incluye_miembros, lista, proyecta},
    contexto::{Contexto, CuentaLeida, id_de_texto, id_texto},
    error::{SCIM_CONTENT_TYPE, ScimError, ScimResult, scim_json_response},
    esquema::{MAX_PAYLOAD, Tipo, lista_completa, resource_type, schema, service_provider_config},
    recursos::{
        EntradaGrupo, EntradaUsuario, Miembros, booleano, escribe_grupo, escribe_usuario,
        representa, valida_atributos,
    },
};
use common::{Server, auth::AccessToken};
use http_proto::{HttpRequest, HttpResponse, HttpSessionData, request::fetch_body};
use hyper::{Method, StatusCode, header};
use registry::schema::enums::Permission;
use serde_json::Value;
use std::time::Instant;
use trc::ScimEvent;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Metodo {
    Get,
    Post,
    Put,
    Patch,
    Delete,
}

/// Petición SCIM ya leída (de HTTP o de una operación de Bulk).
pub struct Peticion {
    pub metodo: Metodo,
    /// Segmentos tras `/scim/v2`.
    pub ruta: Vec<String>,
    pub query: Option<String>,
    pub cuerpo: Option<Value>,
    pub if_match: Option<String>,
    pub if_none_match: Option<String>,
}

/// Respuesta correcta.
pub struct Respuesta {
    pub status: u16,
    pub cuerpo: Option<Value>,
    pub location: Option<String>,
    pub etag: Option<String>,
}

impl Respuesta {
    fn json(status: u16, cuerpo: Value) -> Self {
        Respuesta {
            status,
            cuerpo: Some(cuerpo),
            location: None,
            etag: None,
        }
    }

    fn recurso(status: u16, cuerpo: Value) -> Self {
        let location = cuerpo
            .pointer("/meta/location")
            .and_then(|l| l.as_str())
            .map(|l| l.to_string());
        let etag = cuerpo
            .pointer("/meta/version")
            .and_then(|l| l.as_str())
            .map(|l| l.to_string());
        Respuesta {
            status,
            cuerpo: Some(cuerpo),
            location,
            etag,
        }
    }

    fn into_http_response(self) -> HttpResponse {
        let status = StatusCode::from_u16(self.status).unwrap_or(StatusCode::OK);
        let mut response = match &self.cuerpo {
            Some(cuerpo) => scim_json_response(status, cuerpo),
            None => HttpResponse::new(status),
        };
        if let Some(location) = self.location
            && self.status == 201
        {
            response = response.with_header(header::LOCATION, location);
        }
        if let Some(etag) = self.etag {
            response = response.with_header(header::ETAG, etag);
        }
        response
    }
}

const DESCUBRIMIENTO: [&str; 3] = ["ServiceProviderConfig", "ResourceTypes", "Schemas"];

fn segmentos(ruta: &[String]) -> Option<&[String]> {
    match ruta.first() {
        Some(v) if v.eq_ignore_ascii_case("v2") => Some(&ruta[1..]),
        _ => None,
    }
}

/// ¿Hace falta credencial para esta petición? (no: `OPTIONS`, descubrimiento, `/Me` y rutas desconocidas)
pub fn necesita_credencial(metodo: &Method, ruta: &[String]) -> bool {
    if metodo == Method::OPTIONS {
        return false;
    }
    match segmentos(ruta).and_then(|s| s.first()) {
        Some(primero) => {
            !DESCUBRIMIENTO
                .iter()
                .any(|d| primero.eq_ignore_ascii_case(d))
                && !primero.eq_ignore_ascii_case("Me")
                && matches!(primero.as_str(), "Users" | "Groups" | ".search" | "Bulk")
        }
        None => false,
    }
}

/// Basic no se admite (doc): se rechaza antes de autenticar.
pub fn rechaza_basic(req: &HttpRequest) -> Option<HttpResponse> {
    let valor = req.headers().get(header::AUTHORIZATION)?.to_str().ok()?;
    if valor
        .split_whitespace()
        .next()
        .is_some_and(|esquema| esquema.eq_ignore_ascii_case("basic"))
    {
        Some(
            ScimError::unauthorized(
                "HTTP Basic authentication is not supported, present a bearer token",
            )
            .into_http_response(),
        )
    } else {
        None
    }
}

/// Respuesta SCIM a un fallo de autenticación (401, 429…).
pub fn error_autenticacion(err: trc::Error, session_id: u64) -> HttpResponse {
    let mut scim = ScimError::from(err);
    if scim.status == 400 || scim.status == 404 {
        scim = ScimError::unauthorized("Invalid or missing bearer token");
    }
    trc::event!(
        Scim(scim.event()),
        Code = scim.status as u64,
        Details = scim.detail.clone(),
        SpanId = session_id,
    );
    scim.into_http_response()
}

fn base_url(server: &Server, req: &HttpRequest, session: &HttpSessionData) -> String {
    let publica = server.core.network.http.url_https.trim_end_matches('/');
    if !publica.is_empty() {
        format!("{publica}/scim/v2")
    } else {
        let host = req
            .headers()
            .get(header::HOST)
            .and_then(|h| h.to_str().ok())
            .unwrap_or("localhost");
        format!(
            "{}://{host}/scim/v2",
            if session.is_tls { "https" } else { "http" }
        )
    }
}

/// `If-Match`: sin cabecera, `*` o alguna versión igual (sin distinguir débil/fuerte).
fn cumple_if_match(cabecera: Option<&str>, etag: &str) -> bool {
    let Some(cabecera) = cabecera else {
        return true;
    };
    let limpia = |s: &str| {
        s.trim()
            .trim_start_matches("W/")
            .trim_matches('"')
            .to_string()
    };
    let etag = limpia(etag);
    cabecera
        .split(',')
        .any(|c| c.trim() == "*" || limpia(c) == etag)
}

/// Endpoints de descubrimiento (sin autenticación).
fn descubrimiento(
    base: &str,
    metodo: Metodo,
    ruta: &[String],
    query: Option<&str>,
) -> ScimResult<Respuesta> {
    if metodo != Metodo::Get {
        return Err(ScimError::method_not_allowed("GET"));
    }
    if query.is_some_and(|q| {
        http_proto::form_urlencoded::parse(q.as_bytes())
            .any(|(k, _)| k.eq_ignore_ascii_case("filter"))
    }) {
        return Err(ScimError::forbidden(
            "Filtering is not supported on discovery endpoints",
        ));
    }
    let tipos = [Tipo::Usuario, Tipo::Grupo];
    match (
        ruta[0].as_str(),
        ruta.get(1).map(|s| s.as_str()),
        ruta.len(),
    ) {
        ("ServiceProviderConfig", None, 1) => {
            Ok(Respuesta::json(200, service_provider_config(base)))
        }
        ("ResourceTypes", None, 1) => Ok(Respuesta::json(
            200,
            lista_completa(tipos.iter().map(|t| resource_type(base, *t)).collect()),
        )),
        ("ResourceTypes", Some(id), 2) => tipos
            .iter()
            .find(|t| t.nombre().eq_ignore_ascii_case(id))
            .map(|t| Respuesta::json(200, resource_type(base, *t)))
            .ok_or_else(|| ScimError::not_found(format!("Unknown resource type '{id}'"))),
        ("Schemas", None, 1) => Ok(Respuesta::json(
            200,
            lista_completa(tipos.iter().map(|t| schema(base, *t)).collect()),
        )),
        ("Schemas", Some(urn), 2) => tipos
            .iter()
            .find(|t| t.urn().eq_ignore_ascii_case(urn))
            .map(|t| Respuesta::json(200, schema(base, *t)))
            .ok_or_else(|| ScimError::not_found(format!("Unknown schema '{urn}'"))),
        _ => Err(ScimError::not_found("Unknown SCIM endpoint")),
    }
}

/// Ejecuta una petición sobre recursos (`/Users`, `/Groups`, `/.search`). La usan HTTP y Bulk.
pub async fn ejecuta_recurso(ctx: &Contexto<'_>, pet: Peticion) -> ScimResult<Respuesta> {
    let ruta: Vec<&str> = pet.ruta.iter().map(|s| s.as_str()).collect();
    match ruta.as_slice() {
        [".search"] => {
            if pet.metodo != Metodo::Post {
                return Err(ScimError::method_not_allowed("POST"));
            }
            ctx.exige(Permission::SysAccountGet)?;
            let p = Parametros::de_cuerpo(&exige_cuerpo(&pet)?)?;
            lista(ctx, &[Tipo::Usuario, Tipo::Grupo], p)
                .await
                .map(|r| Respuesta::json(200, r))
        }
        [endpoint] => {
            let tipo = Tipo::de_endpoint(endpoint)
                .ok_or_else(|| ScimError::not_found("Unknown SCIM endpoint"))?;
            match pet.metodo {
                Metodo::Get => {
                    ctx.exige(Permission::SysAccountGet)?;
                    let p = Parametros::de_query(pet.query.as_deref())?;
                    lista(ctx, &[tipo], p)
                        .await
                        .map(|r| Respuesta::json(200, r))
                }
                Metodo::Post => {
                    ctx.exige(Permission::SysAccountCreate)?;
                    let cuerpo = exige_cuerpo(&pet)?;
                    let doc = valida_atributos(tipo, &cuerpo)?;
                    let id = match tipo {
                        Tipo::Usuario => {
                            escribe_usuario(ctx, None, EntradaUsuario::de(&doc)?).await?
                        }
                        Tipo::Grupo => escribe_grupo(ctx, None, EntradaGrupo::de(&doc)?).await?,
                    };
                    let leida = lee(ctx, tipo, id).await?;
                    let doc = representa(ctx, &leida, Miembros::Limitados).await?;
                    Ok(Respuesta::recurso(201, doc))
                }
                _ => Err(ScimError::method_not_allowed("GET, POST")),
            }
        }
        [endpoint, ".search"] => {
            let tipo = Tipo::de_endpoint(endpoint)
                .ok_or_else(|| ScimError::not_found("Unknown SCIM endpoint"))?;
            if pet.metodo != Metodo::Post {
                return Err(ScimError::method_not_allowed("POST"));
            }
            ctx.exige(Permission::SysAccountGet)?;
            let p = Parametros::de_cuerpo(&exige_cuerpo(&pet)?)?;
            lista(ctx, &[tipo], p)
                .await
                .map(|r| Respuesta::json(200, r))
        }
        [endpoint, id] => {
            let tipo = Tipo::de_endpoint(endpoint)
                .ok_or_else(|| ScimError::not_found("Unknown SCIM endpoint"))?;
            let id_num = id_de_texto(id).ok_or_else(|| ScimError::resource_not_found(id))?;
            match pet.metodo {
                Metodo::Get => {
                    ctx.exige(Permission::SysAccountGet)?;
                    let p = Parametros::de_query(pet.query.as_deref())?;
                    let leida = lee(ctx, tipo, id_num).await?;
                    let miembros = if incluye_miembros(&p.atributos, &p.excluidos) {
                        Miembros::Limitados
                    } else {
                        Miembros::Excluidos
                    };
                    let doc = representa(ctx, &leida, miembros).await?;
                    let version = doc
                        .pointer("/meta/version")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string();
                    if let Some(inm) = &pet.if_none_match
                        && inm
                            .split(',')
                            .any(|c| c.trim() == "*" || cumple_if_match(Some(c), &version))
                    {
                        return Ok(Respuesta {
                            status: 304,
                            cuerpo: None,
                            location: None,
                            etag: Some(version),
                        });
                    }
                    let mut r =
                        Respuesta::recurso(200, proyecta(tipo, doc, &p.atributos, &p.excluidos));
                    r.etag = Some(version);
                    Ok(r)
                }
                Metodo::Put => {
                    ctx.exige(Permission::SysAccountUpdate)?;
                    let cuerpo = exige_cuerpo(&pet)?;
                    let leida = lee(ctx, tipo, id_num).await?;
                    let antes = representa(ctx, &leida, Miembros::Todos).await?;
                    comprueba_version(&pet, &antes)?;
                    let doc = valida_atributos(tipo, &cuerpo)?;
                    match tipo {
                        Tipo::Usuario => {
                            escribe_usuario(ctx, Some(&leida), EntradaUsuario::de(&doc)?).await?
                        }
                        Tipo::Grupo => {
                            escribe_grupo(ctx, Some(&leida), EntradaGrupo::de(&doc)?).await?
                        }
                    };
                    despues(ctx, tipo, id_num, &antes).await
                }
                Metodo::Patch => {
                    ctx.exige(Permission::SysAccountUpdate)?;
                    let cuerpo = exige_cuerpo(&pet)?;
                    let leida = lee(ctx, tipo, id_num).await?;
                    let antes = representa(ctx, &leida, Miembros::Todos).await?;
                    comprueba_version(&pet, &antes)?;
                    aplica_patch(ctx, tipo, &leida, &antes, &cuerpo).await?;
                    despues(ctx, tipo, id_num, &antes).await
                }
                Metodo::Delete => {
                    ctx.exige(Permission::SysAccountDestroy)?;
                    let leida = lee(ctx, tipo, id_num).await?;
                    if pet.if_match.is_some() {
                        let antes = representa(ctx, &leida, Miembros::Todos).await?;
                        comprueba_version(&pet, &antes)?;
                    }
                    recursos::borra(ctx, &leida).await?;
                    Ok(Respuesta {
                        status: 204,
                        cuerpo: None,
                        location: None,
                        etag: None,
                    })
                }
                Metodo::Post => Err(ScimError::method_not_allowed("GET, PUT, PATCH, DELETE")),
            }
        }
        _ => Err(ScimError::not_found("Unknown SCIM endpoint")),
    }
}

fn exige_cuerpo(pet: &Peticion) -> ScimResult<Value> {
    pet.cuerpo
        .clone()
        .ok_or_else(|| ScimError::invalid_syntax("The request requires a JSON body"))
}

async fn lee(ctx: &Contexto<'_>, tipo: Tipo, id: u32) -> ScimResult<CuentaLeida> {
    ctx.cuenta(id, Some(tipo))
        .await?
        .ok_or_else(|| ScimError::resource_not_found(&id_texto(id)))
}

fn comprueba_version(pet: &Peticion, antes: &Value) -> ScimResult<()> {
    let version = antes
        .pointer("/meta/version")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    if cumple_if_match(pet.if_match.as_deref(), version) {
        Ok(())
    } else {
        Err(ScimError::precondition_failed())
    }
}

/// Lee el recurso tras un cambio, emite `scim.resource-updated` si cambió y responde 200.
async fn despues(ctx: &Contexto<'_>, tipo: Tipo, id: u32, antes: &Value) -> ScimResult<Respuesta> {
    let leida = lee(ctx, tipo, id).await?;
    let doc = representa(ctx, &leida, Miembros::Limitados).await?;
    if doc.pointer("/meta/version") != antes.pointer("/meta/version") {
        trc::event!(
            Scim(ScimEvent::ResourceUpdated),
            Type = tipo.nombre(),
            Id = id_texto(id),
            AccountId = ctx.token.account_id(),
            SpanId = ctx.session.session_id,
        );
    }
    Ok(Respuesta::recurso(200, doc))
}

/// PATCH: aplica las operaciones a la representación y guarda el resultado como un reemplazo.
async fn aplica_patch(
    ctx: &Contexto<'_>,
    tipo: Tipo,
    leida: &CuentaLeida,
    antes: &Value,
    cuerpo: &Value,
) -> ScimResult<()> {
    let resultado = patch::aplica(tipo, antes.clone(), cuerpo)?;
    for atributo in &resultado.ignorados {
        trc::event!(
            Scim(ScimEvent::AttributeIgnored),
            Type = tipo.nombre(),
            Id = id_texto(leida.id),
            Details = atributo.clone(),
            SpanId = ctx.session.session_id,
        );
    }
    if resultado.tocados.is_empty() {
        return Ok(());
    }
    let mut doc = resultado.doc;
    let tocado = |a: &str| resultado.tocados.contains(a);
    if tipo == Tipo::Usuario {
        let mapa = doc
            .as_object_mut()
            .ok_or_else(|| ScimError::internal("Invalid resource"))?;
        // Dirección principal: sale de userName; cambiarla por `emails` es renombrar (especificación §5.4).
        if tocado("emails") && !tocado("username") {
            let emails = mapa
                .get("emails")
                .and_then(|e| e.as_array())
                .cloned()
                .unwrap_or_default();
            let principales: Vec<&Value> = emails
                .iter()
                .filter(|e| e.get("primary").and_then(booleano) == Some(true))
                .collect();
            let [principal] = principales.as_slice() else {
                return Err(ScimError::invalid_value(
                    "The primary address cannot be removed or made non-primary through 'emails'; it is derived from 'userName'",
                ));
            };
            if principal
                .get("type")
                .and_then(|t| t.as_str())
                .is_some_and(|t| !t.eq_ignore_ascii_case("work"))
            {
                return Err(ScimError::invalid_value(
                    "The type of the primary address cannot be changed; it is derived from 'userName'",
                ));
            }
            if let Some(valor) = principal.get("value").and_then(|v| v.as_str()) {
                mapa.insert("userName".into(), Value::from(valor));
            }
        }
        // displayName y name.formatted son el mismo campo: manda el que tocó el PATCH.
        if tocado("name") && !tocado("displayname") {
            let nombre = mapa.get("name").cloned().unwrap_or(Value::Null);
            let formatted = nombre
                .get("formatted")
                .and_then(|f| f.as_str())
                .map(|s| s.to_string());
            mapa.remove("displayName");
            if let Some(f) = formatted {
                mapa.insert("displayName".into(), Value::from(f));
            }
        } else if tocado("displayname")
            && let Some(n) = mapa.get_mut("name").and_then(|n| n.as_object_mut())
        {
            n.remove("formatted");
        }
        if tocado("preferredlanguage") && !tocado("locale") {
            let pl = mapa.get("preferredLanguage").cloned();
            match pl {
                Some(pl) => mapa.insert("locale".into(), pl),
                None => mapa.remove("locale"),
            };
        }
    }
    let doc = valida_atributos(tipo, &doc)?;
    match tipo {
        Tipo::Usuario => {
            escribe_usuario(ctx, Some(leida), EntradaUsuario::de(&doc)?).await?;
        }
        Tipo::Grupo => {
            escribe_grupo(ctx, Some(leida), EntradaGrupo::de(&doc)?).await?;
        }
    }
    Ok(())
}

/// Entrada HTTP de SCIM (la llama `http` con la ruta tras `/scim` y el token si hizo falta autenticar).
pub trait ScimServidor: Sync + Send {
    fn handle_scim_request(
        &self,
        req: &mut HttpRequest,
        session: &HttpSessionData,
        ruta: Vec<String>,
        token: Option<&AccessToken>,
    ) -> impl Future<Output = HttpResponse> + Send;
}

impl ScimServidor for Server {
    async fn handle_scim_request(
        &self,
        req: &mut HttpRequest,
        session: &HttpSessionData,
        ruta: Vec<String>,
        token: Option<&AccessToken>,
    ) -> HttpResponse {
        let inicio = Instant::now();
        let metodo_http = req.method().clone();
        let path = req.uri().path().to_string();
        let resultado = Box::pin(atiende(self, req, session, ruta, token)).await;
        let (status, response) = match resultado {
            Ok(r) => (r.status, r.into_http_response()),
            Err(err) => {
                trc::event!(
                    Scim(err.event()),
                    Code = err.status as u64,
                    Type = metodo_http.to_string(),
                    Path = path.clone(),
                    Details = err.detail.clone(),
                    Reason = err.scim_type.unwrap_or_default(),
                    SpanId = session.session_id,
                );
                (err.status, err.into_http_response())
            }
        };
        trc::event!(
            Scim(ScimEvent::Request),
            Type = metodo_http.to_string(),
            Path = path,
            Code = status as u64,
            Elapsed = inicio.elapsed(),
            SpanId = session.session_id,
        );
        response
    }
}

async fn atiende(
    server: &Server,
    req: &mut HttpRequest,
    session: &HttpSessionData,
    ruta: Vec<String>,
    token: Option<&AccessToken>,
) -> ScimResult<Respuesta> {
    let ruta: Vec<String> = ruta.into_iter().filter(|s| !s.is_empty()).collect();
    if req.method() == Method::OPTIONS {
        return Ok(Respuesta {
            status: 204,
            cuerpo: None,
            location: None,
            etag: None,
        });
    }
    let Some(segmentos) = segmentos(&ruta).filter(|s| !s.is_empty()) else {
        return Err(ScimError::not_found("Unknown SCIM endpoint"));
    };
    let metodo = match *req.method() {
        Method::GET | Method::HEAD => Metodo::Get,
        Method::POST => Metodo::Post,
        Method::PUT => Metodo::Put,
        Method::PATCH => Metodo::Patch,
        Method::DELETE => Metodo::Delete,
        _ => {
            return Err(ScimError::method_not_allowed(
                "GET, POST, PUT, PATCH, DELETE",
            ));
        }
    };
    let base = base_url(server, req, session);
    let query = req.uri().query().map(|q| q.to_string());

    if DESCUBRIMIENTO.iter().any(|d| segmentos[0] == *d) {
        return descubrimiento(&base, metodo, segmentos, query.as_deref());
    }
    if segmentos[0].eq_ignore_ascii_case("Me") {
        return Err(ScimError::not_implemented(
            "The /Me endpoint is not supported: the SCIM client is not a provisioned resource",
        ));
    }
    let Some(token) = token else {
        return Err(ScimError::not_found("Unknown SCIM endpoint"));
    };
    if !token.has_permission(Permission::ScimAccess) {
        return Err(ScimError::forbidden(
            "The provisioning account lacks the 'scimAccess' permission",
        ));
    }

    let cuerpo = if matches!(metodo, Metodo::Post | Metodo::Put | Metodo::Patch) {
        let bytes = fetch_body(req, MAX_PAYLOAD, session.session_id)
            .await
            .ok_or_else(|| ScimError::payload_too_large(MAX_PAYLOAD))?;
        if bytes.is_empty() {
            None
        } else {
            Some(serde_json::from_slice::<Value>(&bytes).map_err(|err| {
                ScimError::invalid_syntax(format!("The request body is not valid JSON: {err}"))
            })?)
        }
    } else {
        None
    };
    let cabecera = |nombre: header::HeaderName| {
        req.headers()
            .get(nombre)
            .and_then(|v| v.to_str().ok())
            .map(|v| v.to_string())
    };
    let peticion = Peticion {
        metodo,
        ruta: segmentos.to_vec(),
        query,
        cuerpo,
        if_match: cabecera(header::IF_MATCH),
        if_none_match: cabecera(header::IF_NONE_MATCH),
    };

    let ctx = Contexto::nuevo(server, token, session, base).await?;
    if peticion.ruta[0] == "Bulk" {
        if peticion.ruta.len() != 1 {
            return Err(ScimError::not_found("Unknown SCIM endpoint"));
        }
        if metodo != Metodo::Post {
            return Err(ScimError::method_not_allowed("POST"));
        }
        let cuerpo = exige_cuerpo(&peticion)?;
        return bulk::bulk(&ctx, &cuerpo)
            .await
            .map(|r| Respuesta::json(200, r));
    }
    ejecuta_recurso(&ctx, peticion).await
}

/// Tipo de contenido de las respuestas SCIM (para quien lo necesite fuera del crate).
pub const CONTENT_TYPE: &str = SCIM_CONTENT_TYPE;

#[cfg(test)]
mod pruebas {
    use super::*;

    #[test]
    fn credencial_y_versiones() {
        let r = |s: &[&str]| s.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert!(!necesita_credencial(
            &Method::GET,
            &r(&["v2", "ServiceProviderConfig"])
        ));
        assert!(!necesita_credencial(
            &Method::GET,
            &r(&["v2", "Schemas", "urn:x"])
        ));
        assert!(!necesita_credencial(&Method::OPTIONS, &r(&["v2", "Users"])));
        assert!(!necesita_credencial(&Method::GET, &r(&["v2", "Me"])));
        assert!(necesita_credencial(&Method::GET, &r(&["v2", "Users"])));
        assert!(necesita_credencial(&Method::POST, &r(&["v2", "Bulk"])));
        assert!(necesita_credencial(&Method::POST, &r(&["v2", ".search"])));
        assert!(cumple_if_match(None, "W/\"a\""));
        assert!(cumple_if_match(Some("*"), "W/\"a\""));
        assert!(cumple_if_match(Some("W/\"b\", W/\"a\""), "W/\"a\""));
        assert!(cumple_if_match(Some("\"a\""), "W/\"a\""));
        assert!(!cumple_if_match(Some("W/\"b\""), "W/\"a\""));
    }
}
