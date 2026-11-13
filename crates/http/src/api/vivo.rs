/*
 * SPDX-FileCopyrightText: 2026 bezenti
 *
 * SPDX-License-Identifier: AGPL-3.0-only
 */

//! Telemetría en vivo por HTTP (Server-Sent Events). Edición bezenti, fase 5.
//! Especificación: `bezenti/especificaciones/telemetria.md`, §7.
//!
//! - `GET /api/token/tracing` y `/api/token/metrics`: token de 60 s para clientes que no pueden poner cabeceras
//!   (`EventSource`), válido solo para su flujo.
//! - `GET /api/live/tracing[?filter=…&<clave>=…]`: eventos por lotes (como mucho uno por segundo), con un `ping`
//!   al empezar y cada 30 s sin tráfico. Un usuario de inquilino solo recibe las sesiones de su ámbito.
//! - `GET /api/live/metrics[?metrics=a,b&interval=N]`: valores actuales del nodo cada `N` s (30 por defecto).
//!   Son del servidor entero: un usuario de inquilino no puede usarlo.

use common::{
    Server,
    auth::{AccessToken, oauth::GrantType},
    bezenti::{
        inquilino::TenantScope,
        telemetria::vivo::{FiltroVivo, SelectorVivo, SuscripcionVivo},
    },
};
use http_body_util::{StreamBody, combinators::BoxBody};
use http_proto::HttpResponse;
use hyper::{
    StatusCode,
    body::{Bytes, Frame},
};
use registry::schema::enums::Permission;
use std::time::{Duration, Instant};
use trc::{Collector, MetricType};
use utils::url_params::UrlParams;

const PING: Duration = Duration::from_secs(30);
const LOTE: Duration = Duration::from_secs(1);

/// Ámbito de inquilino de quien abre el flujo (`None`: operador). El token de un solo uso no lleva el inquilino,
/// así que se mira siempre en la cuenta.
async fn ambito(server: &Server, access_token: &AccessToken) -> trc::Result<Option<TenantScope>> {
    let tenant = match access_token.tenant_id() {
        Some(t) => Some(t),
        None => server
            .try_account(access_token.account_id())
            .await?
            .and_then(|c| c.tenant_id),
    };
    match tenant {
        Some(t) => server.tenant_scope(t).await.map(Some),
        None => Ok(None),
    }
}

/// `GET /api/token/{tracing,metrics}`.
pub async fn token(
    server: &Server,
    access_token: &AccessToken,
    metricas: bool,
) -> trc::Result<HttpResponse> {
    let (grant, permiso) = if metricas {
        (GrantType::LiveMetrics, Permission::LiveMetrics)
    } else {
        (GrantType::LiveTracing, Permission::LiveTracing)
    };
    access_token.enforce_permission(permiso)?;
    if metricas && ambito(server, access_token).await?.is_some() {
        return Err(trc::SecurityEvent::Unauthorized.into_err().details(
            "Live metrics are server-wide and only available to the server administrator",
        ));
    }
    let account_id = access_token.account_id();
    Ok(HttpResponse::new(StatusCode::OK)
        .with_no_cache()
        .with_text_body(
            server
                .encode_access_token(
                    grant,
                    account_id,
                    server.account(account_id).await?.name(),
                    60,
                    None,
                    None,
                )
                .await?,
        ))
}

fn evento_sse(nombre: &str, datos: &str) -> Frame<Bytes> {
    Frame::data(Bytes::from(format!("event: {nombre}\ndata: {datos}\n\n")))
}

fn sse(cuerpo: BoxBody<Bytes, hyper::Error>) -> HttpResponse {
    HttpResponse::new(StatusCode::OK)
        .with_content_type("text/event-stream")
        .with_cache_control("no-store")
        .with_stream_body(cuerpo)
}

/// `GET /api/live/tracing`.
pub async fn tracing(
    server: &Server,
    access_token: &AccessToken,
    query: &str,
) -> trc::Result<HttpResponse> {
    access_token.enforce_permission(Permission::LiveTracing)?;
    let ambito = ambito(server, access_token).await?;
    let mut selector = SelectorVivo::new(FiltroVivo::parse(query), ambito);
    let mut suscripcion = SuscripcionVivo::nueva();
    let server = server.clone();

    // El trabajo (que consulta las cachés y el almacén) va en su propia tarea: el cuerpo de la respuesta solo lee
    // de un canal, así cumple `Send + Sync` con cualquier almacén. Al cerrarse la conexión se suelta el canal y
    // la tarea termina (y con ella la suscripción).
    let (tx, rx) = tokio::sync::mpsc::channel::<Frame<Bytes>>(16);
    tokio::spawn(async move {
        if tx
            .send(evento_sse(
                "ping",
                &format!("{{\"interval\": {}}}", PING.as_millis()),
            ))
            .await
            .is_err()
        {
            return;
        }
        let mut pendientes = Vec::new();
        let mut ultimo = Instant::now();
        loop {
            let espera = if pendientes.is_empty() {
                PING.saturating_sub(ultimo.elapsed())
            } else {
                LOTE.saturating_sub(ultimo.elapsed())
            };
            tokio::select! {
                lote = suscripcion.rx.recv() => match lote {
                    Some(lote) => pendientes.extend(selector.seleccionar(&server, lote).await),
                    None => break,
                },
                _ = tokio::time::sleep(espera) => {}
                _ = tx.closed() => break,
            }
            let marco = if !pendientes.is_empty() && ultimo.elapsed() >= LOTE {
                let datos = serde_json::to_string(&pendientes).unwrap_or_default();
                pendientes.clear();
                evento_sse("trace", &datos)
            } else if pendientes.is_empty() && ultimo.elapsed() >= PING {
                evento_sse("ping", &format!("{{\"interval\": {}}}", PING.as_millis()))
            } else {
                continue;
            };
            ultimo = Instant::now();
            if tx.send(marco).await.is_err() {
                break;
            }
        }
    });
    Ok(sse(canal(rx)))
}

/// Cuerpo SSE que lee los marcos de un canal.
fn canal(mut rx: tokio::sync::mpsc::Receiver<Frame<Bytes>>) -> BoxBody<Bytes, hyper::Error> {
    BoxBody::new(StreamBody::new(async_stream::stream! {
        while let Some(marco) = rx.recv().await {
            yield Ok(marco);
        }
    }))
}

/// Valores actuales de las métricas (`None` = todas).
fn valores(nombres: &Option<Vec<MetricType>>) -> serde_json::Value {
    let quiere = |m: &MetricType| nombres.as_ref().is_none_or(|n| n.contains(m));
    let mut lista = Vec::new();
    for c in Collector::collect_counters(true) {
        if let Some(m) = common::bezenti::telemetria::metricas::metrica_de_evento(c.id())
            && quiere(&m)
        {
            lista.push(
                serde_json::json!({"metric": m.as_str(), "@type": "Counter", "count": c.value()}),
            );
        }
    }
    for g in Collector::collect_gauges(true) {
        if quiere(&g.id()) {
            lista.push(
                serde_json::json!({"metric": g.id().as_str(), "@type": "Gauge", "count": g.get()}),
            );
        }
    }
    for h in Collector::collect_histograms(true) {
        if quiere(&h.id()) {
            lista.push(serde_json::json!({
                "metric": h.id().as_str(), "@type": "Histogram", "count": h.count(), "sum": h.sum()
            }));
        }
    }
    serde_json::Value::Array(lista)
}

/// `GET /api/live/metrics`.
pub async fn metrics(
    server: &Server,
    access_token: &AccessToken,
    query: Option<&str>,
) -> trc::Result<HttpResponse> {
    access_token.enforce_permission(Permission::LiveMetrics)?;
    if ambito(server, access_token).await?.is_some() {
        return Err(trc::SecurityEvent::Unauthorized.into_err().details(
            "Live metrics are server-wide and only available to the server administrator",
        ));
    }
    let params = UrlParams::new(query);
    let intervalo = Duration::from_secs(
        params
            .parse::<u64>("interval")
            .filter(|i| *i >= 1)
            .unwrap_or(30),
    );
    let nombres = params.get("metrics").map(|m| {
        m.split(',')
            .filter_map(|n| MetricType::parse(n.trim()))
            .collect::<Vec<_>>()
    });
    let server = server.clone();

    let (tx, rx) = tokio::sync::mpsc::channel::<Frame<Bytes>>(4);
    tokio::spawn(async move {
        loop {
            server.inner.refresh_live_gauges();
            let datos = valores(&nombres).to_string();
            if tx.send(evento_sse("metrics", &datos)).await.is_err() {
                break;
            }
            tokio::select! {
                _ = tokio::time::sleep(intervalo) => {}
                _ = tx.closed() => break,
            }
        }
    });
    Ok(sse(canal(rx)))
}
