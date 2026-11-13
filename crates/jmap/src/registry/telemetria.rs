/*
 * SPDX-FileCopyrightText: 2026 bezenti
 *
 * SPDX-License-Identifier: AGPL-3.0-only
 */

//! API del historial de telemetría: `x:Trace/get`, `x:Trace/query`, `x:Metric/get`, `x:Metric/query`.
//! Edición bezenti, fase 5. Especificación: `bezenti/especificaciones/telemetria.md`, §5.
//!
//! Quién ve qué: el operador, todo; un usuario de inquilino (con los permisos `sysTrace*`/`sysMetric*`), solo las
//! trazas en las que participa su ámbito (su inquilino y sus descendientes) y, como métricas, los contadores de
//! los eventos de esas trazas. Lo que queda fuera del ámbito no existe: `notFound` y consultas vacías.

use crate::{
    api::query::QueryResponseBuilder,
    registry::{
        bezenti::TenantAccess,
        mapping::{RegistryGetResponse, RegistryQueryResponse},
        query::{RegistryQueryFilters, with_page_position},
    },
};
use common::bezenti::telemetria::{
    metricas::FiltroMetricas,
    trazas::{FiltroTrazas, Tipo},
};
use compact_str::format_compact;
use jmap_proto::types::state::State;
use registry::{jmap::IntoValue, schema::prelude::Property, types::datetime::UTCDateTime};
use std::str::FromStr;
use store::registry::RegistryFilterOp;
use trc::{EventType, MetricType};
use types::id::Id;

/// Aplica un filtro de fecha (`timestamp`, `timestampIsGreaterThan`…) a un rango de segundos inclusivo.
fn filtro_fecha(
    op: RegistryFilterOp,
    valor: &serde_json::Value,
    desde: &mut Option<u64>,
    hasta: &mut Option<u64>,
) -> bool {
    let Some(texto) = valor.as_str() else {
        return false;
    };
    // Fecha sola (AAAA-MM-DD): el día entero.
    let (inicio, fin) = if texto.len() == 10 {
        match UTCDateTime::from_str(&format!("{texto}T00:00:00Z")) {
            Ok(d) => (
                d.timestamp().max(0) as u64,
                d.timestamp().max(0) as u64 + 86_399,
            ),
            Err(_) => return false,
        }
    } else {
        match UTCDateTime::from_str(texto) {
            Ok(d) => (d.timestamp().max(0) as u64, d.timestamp().max(0) as u64),
            Err(_) => return false,
        }
    };
    let mas_tarde =
        |actual: &mut Option<u64>, v: u64| *actual = Some(actual.map_or(v, |a| a.max(v)));
    let mas_pronto =
        |actual: &mut Option<u64>, v: u64| *actual = Some(actual.map_or(v, |a| a.min(v)));
    match op {
        RegistryFilterOp::Equal | RegistryFilterOp::TextMatch => {
            mas_tarde(desde, inicio);
            mas_pronto(hasta, fin);
        }
        RegistryFilterOp::GreaterThan => mas_tarde(desde, fin + 1),
        RegistryFilterOp::GreaterEqualThan => mas_tarde(desde, inicio),
        RegistryFilterOp::LowerThan => mas_pronto(hasta, inicio.saturating_sub(1)),
        RegistryFilterOp::LowerEqualThan => mas_pronto(hasta, fin),
    }
    true
}

/// Id de cola tal como lo da `x:QueuedMessage` (id JMAP) o en decimal.
fn id_cola(valor: &serde_json::Value) -> Option<u64> {
    match valor {
        serde_json::Value::String(s) => Id::from_str(s)
            .ok()
            .map(|id| id.id())
            .or_else(|| s.parse::<u64>().ok()),
        serde_json::Value::Number(n) => n.as_u64(),
        _ => None,
    }
}

fn orden(property: Property) -> trc::Result<()> {
    match property {
        Property::Id | Property::Timestamp => Ok(()),
        property => Err(trc::JmapEvent::UnsupportedSort
            .into_err()
            .details(format_compact!(
                "Property {} is not supported for sorting",
                property
            ))),
    }
}

fn respuesta(req: &RegistryQueryResponse<'_>, ids: Vec<u64>) -> trc::Result<QueryResponseBuilder> {
    let mut response = with_page_position(
        QueryResponseBuilder::new(
            ids.len(),
            req.server.core.jmap.query_max_results,
            State::Initial,
            &req.request,
        ),
        None,
    );
    for id in ids {
        if !response.add_id(Id::from(id)) {
            break;
        }
    }
    Ok(response)
}

pub(crate) async fn trace_get<'x>(
    mut get: RegistryGetResponse<'x>,
) -> trc::Result<RegistryGetResponse<'x>> {
    let server = get.server;
    let ambito = get.tenant.scope.clone();
    let ids = match get.ids.take() {
        Some(ids) => ids,
        None => server
            .consultar_trazas(&FiltroTrazas::default(), ambito.as_ref())
            .await?
            .into_iter()
            .take(server.core.jmap.get_max_objects)
            .map(Id::from)
            .collect(),
    };
    for id in ids {
        match server.traza(id.id()).await? {
            Some((inquilinos, traza))
                if ambito
                    .as_ref()
                    .is_none_or(|a| inquilinos.iter().any(|t| a.contains(*t))) =>
            {
                get.insert(id, traza.into_value());
            }
            _ => get.not_found(id),
        }
    }
    Ok(get)
}

pub(crate) async fn trace_query(
    mut req: RegistryQueryResponse<'_>,
    tenant: &TenantAccess,
) -> trc::Result<QueryResponseBuilder> {
    let mut filtro = FiltroTrazas::default();
    let mut vacio = false;
    req.request
        .extract_filters(|property, op, value| match property {
            Property::Text => {
                if let Some(t) = value.as_str() {
                    let t = t.trim().to_lowercase();
                    if !t.is_empty() {
                        filtro.texto = Some(t);
                    }
                }
                true
            }
            Property::Timestamp => filtro_fecha(op, &value, &mut filtro.desde, &mut filtro.hasta),
            Property::QueueId => {
                match id_cola(&value) {
                    Some(c) => filtro.cola = Some(c),
                    None => vacio = true,
                }
                true
            }
            Property::Event => {
                match value
                    .as_str()
                    .and_then(EventType::parse)
                    .and_then(Tipo::por_evento)
                {
                    Some(t) => filtro.tipo = Some(t),
                    None => vacio = true,
                }
                true
            }
            _ => false,
        })?;
    let params = req
        .request
        .extract_parameters(req.server.core.jmap.query_max_results, None)?;
    orden(params.sort_by)?;
    filtro.ascendente = params.sort_ascending;
    let ids = if vacio {
        Vec::new()
    } else {
        req.server
            .consultar_trazas(&filtro, tenant.scope.as_ref())
            .await?
    };
    respuesta(&req, ids)
}

pub(crate) async fn metric_get<'x>(
    mut get: RegistryGetResponse<'x>,
) -> trc::Result<RegistryGetResponse<'x>> {
    let server = get.server;
    let ambito = get.tenant.scope.clone();
    let ids = match get.ids.take() {
        Some(ids) => ids,
        None => server
            .consultar_metricas(&FiltroMetricas::default(), ambito.as_ref())
            .await?
            .into_iter()
            .take(server.core.jmap.get_max_objects)
            .map(Id::from)
            .collect(),
    };
    for id in ids {
        match server.metrica(id.id(), ambito.as_ref()).await? {
            Some(punto) => get.insert(id, punto.into_value()),
            None => get.not_found(id),
        }
    }
    Ok(get)
}

pub(crate) async fn metric_query(
    mut req: RegistryQueryResponse<'_>,
    tenant: &TenantAccess,
) -> trc::Result<QueryResponseBuilder> {
    let mut filtro = FiltroMetricas::default();
    let mut vacio = false;
    req.request
        .extract_filters(|property, op, value| match property {
            Property::NodeId => {
                match value.as_u64() {
                    Some(n) if n <= u16::MAX as u64 => filtro.nodo = Some(n as u16),
                    _ => vacio = true,
                }
                true
            }
            Property::Metric => {
                match value.as_str().and_then(MetricType::parse) {
                    Some(m) => filtro.metricas.push(m),
                    None => vacio = true,
                }
                true
            }
            Property::Timestamp => filtro_fecha(op, &value, &mut filtro.desde, &mut filtro.hasta),
            _ => false,
        })?;
    let params = req
        .request
        .extract_parameters(req.server.core.jmap.query_max_results, None)?;
    orden(params.sort_by)?;
    filtro.ascendente = params.sort_ascending;
    let ids = if vacio {
        Vec::new()
    } else {
        req.server
            .consultar_metricas(&filtro, tenant.scope.as_ref())
            .await?
    };
    respuesta(&req, ids)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fechas() {
        let (mut d, mut h) = (None, None);
        assert!(filtro_fecha(
            RegistryFilterOp::Equal,
            &serde_json::json!("2026-11-13"),
            &mut d,
            &mut h
        ));
        assert_eq!(h.unwrap() - d.unwrap(), 86_399);
        let (mut d, mut h) = (None, None);
        assert!(filtro_fecha(
            RegistryFilterOp::GreaterThan,
            &serde_json::json!("2026-11-13T10:00:00Z"),
            &mut d,
            &mut h
        ));
        assert!(d.is_some() && h.is_none());
        assert!(!filtro_fecha(
            RegistryFilterOp::Equal,
            &serde_json::json!(5),
            &mut d,
            &mut h
        ));
    }
}
