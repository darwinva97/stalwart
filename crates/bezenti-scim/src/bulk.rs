/*
 * SPDX-FileCopyrightText: 2026 bezenti
 *
 * SPDX-License-Identifier: AGPL-3.0-only
 */

//! `POST /Bulk` (RFC 7644 §3.7): operaciones en orden, `failOnErrors`, `bulkId` y referencias `bulkId:<id>`.
//! Especificación §10.

use crate::{
    Metodo, Peticion,
    contexto::Contexto,
    ejecuta_recurso,
    error::{ScimError, ScimResult},
    esquema::{MAX_BULK_OPERATIONS, URN_BULK_REQUEST, URN_BULK_RESPONSE},
    recursos::campo,
};
use serde_json::{Value, json};
use std::collections::HashMap;
use trc::ScimEvent;

/// Cambia cada cadena `bulkId:<id>` por el id creado. Devuelve el primer `bulkId` sin resolver.
fn resuelve(valor: &mut Value, creados: &HashMap<String, String>) -> Option<String> {
    match valor {
        Value::String(s) => {
            if let Some(bulk_id) = s.strip_prefix("bulkId:") {
                match creados.get(bulk_id) {
                    Some(id) => *s = id.clone(),
                    None => return Some(bulk_id.to_string()),
                }
            }
            None
        }
        Value::Array(lista) => lista.iter_mut().find_map(|v| resuelve(v, creados)),
        Value::Object(mapa) => mapa.values_mut().find_map(|v| resuelve(v, creados)),
        _ => None,
    }
}

pub async fn bulk(ctx: &Contexto<'_>, cuerpo: &Value) -> ScimResult<Value> {
    let Some(mapa) = cuerpo.as_object() else {
        return Err(ScimError::invalid_syntax(
            "The request body must be a JSON object",
        ));
    };
    let schemas_ok = campo(mapa, "schemas")
        .and_then(|s| s.as_array())
        .is_some_and(|s| {
            s.iter().any(|u| {
                u.as_str()
                    .is_some_and(|u| u.eq_ignore_ascii_case(URN_BULK_REQUEST))
            })
        });
    if !schemas_ok {
        return Err(ScimError::invalid_syntax(format!(
            "The 'schemas' attribute must be [\"{URN_BULK_REQUEST}\"]"
        )));
    }
    let Some(operaciones) = campo(mapa, "Operations").and_then(|o| o.as_array()) else {
        return Err(ScimError::invalid_syntax(
            "The 'Operations' attribute is missing",
        ));
    };
    if operaciones.len() > MAX_BULK_OPERATIONS {
        return Err(ScimError::new(
            413,
            None,
            format!("A bulk request may contain at most {MAX_BULK_OPERATIONS} operations"),
        ));
    }
    let fail_on_errors =
        match campo(mapa, "failOnErrors") {
            None => None,
            Some(v) => Some(v.as_u64().filter(|n| *n > 0).ok_or_else(|| {
                ScimError::invalid_value("failOnErrors must be a positive integer")
            })? as usize),
        };

    let mut creados: HashMap<String, String> = HashMap::new();
    let mut respuestas = Vec::with_capacity(operaciones.len());
    let (mut correctas, mut errores) = (0u64, 0usize);
    for operacion in operaciones {
        let Some(op) = operacion.as_object() else {
            return Err(ScimError::invalid_syntax(
                "Each operation must be a JSON object",
            ));
        };
        let metodo_texto = campo(op, "method")
            .and_then(|m| m.as_str())
            .unwrap_or_default();
        let bulk_id = campo(op, "bulkId")
            .and_then(|b| b.as_str())
            .map(|b| b.to_string());
        let mut salida = json!({ "method": metodo_texto });
        if let Some(b) = &bulk_id {
            salida["bulkId"] = Value::from(b.as_str());
        }

        let resultado: ScimResult<crate::Respuesta> = async {
            let metodo = match metodo_texto.to_ascii_uppercase().as_str() {
                "POST" => Metodo::Post,
                "PUT" => Metodo::Put,
                "PATCH" => Metodo::Patch,
                "DELETE" => Metodo::Delete,
                _ => {
                    return Err(ScimError::invalid_syntax(format!(
                        "Unsupported bulk method '{metodo_texto}'"
                    )));
                }
            };
            if metodo == Metodo::Post && bulk_id.is_none() {
                return Err(ScimError::invalid_syntax("A POST operation requires a bulkId"));
            }
            let path = campo(op, "path")
                .and_then(|p| p.as_str())
                .ok_or_else(|| ScimError::invalid_syntax("Each operation requires a path"))?;
            let mut ruta = Vec::new();
            for segmento in path.split('/').filter(|s| !s.is_empty()) {
                match segmento.strip_prefix("bulkId:") {
                    Some(b) => match creados.get(b) {
                        Some(id) => ruta.push(id.clone()),
                        None => {
                            return Err(ScimError::conflict(format!(
                                "The bulkId '{b}' does not refer to a resource created earlier in this request"
                            )));
                        }
                    },
                    None => ruta.push(segmento.to_string()),
                }
            }
            let mut datos = campo(op, "data").cloned();
            if let Some(datos) = &mut datos
                && let Some(pendiente) = resuelve(datos, &creados)
            {
                return Err(ScimError::conflict(format!(
                    "The bulkId '{pendiente}' does not refer to a resource created earlier in this request"
                )));
            }
            if ruta.first().is_some_and(|r| r.eq_ignore_ascii_case("Bulk")) {
                return Err(ScimError::invalid_value("Bulk operations cannot be nested"));
            }
            ejecuta_recurso(
                ctx,
                Peticion {
                    metodo,
                    ruta,
                    query: None,
                    cuerpo: datos,
                    if_match: campo(op, "version").and_then(|v| v.as_str()).map(|v| v.to_string()),
                    if_none_match: None,
                },
            )
            .await
        }
        .await;

        match resultado {
            Ok(r) => {
                correctas += 1;
                if let (Some(b), Some(id)) = (
                    &bulk_id,
                    r.cuerpo
                        .as_ref()
                        .and_then(|c| c.get("id"))
                        .and_then(|i| i.as_str()),
                ) {
                    creados.insert(b.clone(), id.to_string());
                }
                if let Some(location) = &r.location {
                    salida["location"] = Value::from(location.as_str());
                }
                if let Some(etag) = &r.etag {
                    salida["version"] = Value::from(etag.as_str());
                }
                salida["status"] = Value::from(r.status.to_string());
            }
            Err(err) => {
                errores += 1;
                salida["status"] = Value::from(err.status.to_string());
                salida["response"] = err.to_json();
            }
        }
        respuestas.push(salida);
        if fail_on_errors.is_some_and(|max| errores >= max) {
            trc::event!(
                Scim(ScimEvent::BulkAborted),
                Total = operaciones.len() as u64,
                TotalSuccesses = correctas,
                TotalFailures = errores as u64,
                SpanId = ctx.session.session_id,
            );
            break;
        }
    }
    trc::event!(
        Scim(ScimEvent::BulkProcessed),
        Total = operaciones.len() as u64,
        TotalSuccesses = correctas,
        TotalFailures = errores as u64,
        SpanId = ctx.session.session_id,
    );
    Ok(json!({
        "schemas": [URN_BULK_RESPONSE],
        "Operations": respuestas,
    }))
}

#[cfg(test)]
mod pruebas {
    use super::*;

    #[test]
    fn referencias_bulk_id() {
        let mut creados = HashMap::new();
        creados.insert("u1".to_string(), "b".to_string());
        let mut datos = json!({"members": [{"value": "bulkId:u1"}, {"value": "c"}]});
        assert_eq!(resuelve(&mut datos, &creados), None);
        assert_eq!(datos["members"][0]["value"], "b");
        let mut datos = json!({"members": [{"value": "bulkId:u9"}]});
        assert_eq!(resuelve(&mut datos, &creados), Some("u9".to_string()));
    }
}
