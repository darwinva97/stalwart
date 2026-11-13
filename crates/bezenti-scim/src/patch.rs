/*
 * SPDX-FileCopyrightText: 2026 bezenti
 *
 * SPDX-License-Identifier: AGPL-3.0-only
 */

//! PATCH (RFC 7644 §3.5.2) sobre la representación JSON de un recurso.
//!
//! Decisión bezenti (especificación §8): las operaciones se aplican, en orden, a la representación SCIM actual
//! del recurso y el resultado se guarda como un reemplazo. Se recuerda qué atributos de primer nivel tocó el PATCH
//! (para decidir entre `displayName` y `name.formatted`, y para el renombrado por la dirección principal).
//! Incluye las rarezas de los clientes: `op` con mayúscula (Entra), `"False"` como cadena (Entra), `replace` sin
//! `path` con un objeto de atributos (Okta) cuyas claves pueden ser rutas (`name.givenName`), y `remove` de
//! `members` con una lista de valores (Entra).

use crate::{
    error::{ScimError, ScimResult},
    esquema::{Tipo, URN_ENTERPRISE, URN_PATCH, conocido_no_guardado, guardado},
    filtro::{Filtro, Op, RutaAtributo, RutaPatch, busca_sub},
    recursos::campo,
};
use serde_json::{Map, Value};
use std::collections::BTreeSet;

pub struct ResultadoPatch {
    pub doc: Value,
    /// Atributos de primer nivel (en minúsculas) que cambió alguna operación.
    pub tocados: BTreeSet<String>,
    /// Atributos aceptados pero no guardados que se saltaron (evento `scim.attribute-ignored`).
    pub ignorados: Vec<String>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Operacion {
    Add,
    Replace,
    Remove,
}

/// Nombre canónico de un atributo de primer nivel (para escribirlo en la representación).
fn canonico(tipo: Tipo, nombre: &str) -> &'static str {
    match (tipo, nombre) {
        (_, "externalid") => "externalId",
        (_, "displayname") => "displayName",
        (Tipo::Usuario, "username") => "userName",
        (Tipo::Usuario, "name") => "name",
        (Tipo::Usuario, "active") => "active",
        (Tipo::Usuario, "emails") => "emails",
        (Tipo::Usuario, "locale") => "locale",
        (Tipo::Usuario, "preferredlanguage") => "preferredLanguage",
        (Tipo::Usuario, "timezone") => "timezone",
        (Tipo::Grupo, "members") => "members",
        _ => "",
    }
}

fn es_multivalor(tipo: Tipo, nombre: &str) -> bool {
    matches!(
        (tipo, nombre),
        (Tipo::Usuario, "emails") | (Tipo::Grupo, "members")
    )
}

fn es_complejo(tipo: Tipo, nombre: &str) -> bool {
    matches!((tipo, nombre), (Tipo::Usuario, "name"))
}

fn solo_lectura(tipo: Tipo, nombre: &str) -> bool {
    matches!(nombre, "id" | "meta" | "schemas") || (tipo == Tipo::Usuario && nombre == "groups")
}

/// Clave real (con sus mayúsculas) de un atributo en un objeto.
fn clave_real(mapa: &Map<String, Value>, nombre: &str) -> Option<String> {
    mapa.keys()
        .find(|k| k.eq_ignore_ascii_case(nombre))
        .cloned()
}

/// ¿Son el mismo elemento de un atributo multivalor? (mismo `value`; direcciones sin distinguir mayúsculas)
fn mismo_elemento(a: &Value, b: &Value) -> bool {
    let valor = |v: &Value| -> Option<String> {
        match v {
            Value::Object(_) => busca_sub(v, "value")
                .and_then(|x| x.as_str())
                .map(|s| s.to_lowercase()),
            Value::String(s) => Some(s.to_lowercase()),
            _ => None,
        }
    };
    match (valor(a), valor(b)) {
        (Some(x), Some(y)) => x == y,
        _ => a == b,
    }
}

fn como_lista(valor: Value) -> Vec<Value> {
    match valor {
        Value::Array(lista) => lista,
        Value::Null => Vec::new(),
        otro => vec![otro],
    }
}

/// Mezcla los subatributos de `valor` en `destino` (sin distinguir mayúsculas en las claves).
fn mezcla(destino: &mut Value, valor: Value) {
    match (destino.as_object_mut(), valor) {
        (Some(d), Value::Object(v)) => {
            for (k, x) in v {
                let clave = clave_real(d, &k).unwrap_or(k);
                d.insert(clave, x);
            }
        }
        (_, valor) => *destino = valor,
    }
}

/// Valores fijados por los términos `eq` de un filtro (para crear el elemento cuando no hay ninguno).
fn valores_de_filtro(filtro: &Filtro, out: &mut Map<String, Value>) {
    match filtro {
        Filtro::Comparacion {
            ruta,
            op: Op::Eq,
            valor,
        } => {
            out.insert(ruta.nombre.clone(), valor.clone());
        }
        Filtro::Y(a, b) => {
            valores_de_filtro(a, out);
            valores_de_filtro(b, out);
        }
        _ => {}
    }
}

/// Aplica el cuerpo de un PATCH a la representación `doc`.
pub fn aplica(tipo: Tipo, mut doc: Value, cuerpo: &Value) -> ScimResult<ResultadoPatch> {
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
                    .is_some_and(|u| u.eq_ignore_ascii_case(URN_PATCH))
            })
        });
    if !schemas_ok {
        return Err(ScimError::invalid_syntax(format!(
            "The 'schemas' attribute must be [\"{URN_PATCH}\"]"
        )));
    }
    let Some(operaciones) = campo(mapa, "Operations").and_then(|o| o.as_array()) else {
        return Err(ScimError::invalid_syntax(
            "The 'Operations' attribute is missing",
        ));
    };
    if operaciones.is_empty() {
        return Err(ScimError::invalid_syntax("The 'Operations' list is empty"));
    }
    let mut resultado = ResultadoPatch {
        doc: Value::Null,
        tocados: BTreeSet::new(),
        ignorados: Vec::new(),
    };
    for operacion in operaciones {
        let Some(op) = operacion.as_object() else {
            return Err(ScimError::invalid_syntax(
                "Each operation must be a JSON object",
            ));
        };
        let nombre_op = campo(op, "op").and_then(|o| o.as_str()).unwrap_or_default();
        let tipo_op = match nombre_op.to_ascii_lowercase().as_str() {
            "add" => Operacion::Add,
            "replace" => Operacion::Replace,
            "remove" => Operacion::Remove,
            _ => {
                return Err(ScimError::invalid_syntax(format!(
                    "Unsupported PATCH operation '{nombre_op}' (use add, replace or remove)"
                )));
            }
        };
        let valor = campo(op, "value").cloned();
        match campo(op, "path")
            .and_then(|p| p.as_str())
            .filter(|p| !p.trim().is_empty())
        {
            Some(path) => {
                let ruta = RutaPatch::parse(path).map_err(|e| ScimError::invalid_path(e.0))?;
                aplica_ruta(tipo, &mut doc, tipo_op, &ruta, valor, &mut resultado)?;
            }
            None => {
                if tipo_op == Operacion::Remove {
                    return Err(ScimError::no_target("A remove operation requires a path"));
                }
                let Some(Value::Object(atributos)) = valor else {
                    return Err(ScimError::invalid_value(
                        "An operation without a path requires an object value",
                    ));
                };
                for (clave, v) in atributos {
                    if clave.eq_ignore_ascii_case("schemas") {
                        continue;
                    }
                    if clave.eq_ignore_ascii_case(URN_ENTERPRISE) && tipo == Tipo::Usuario {
                        resultado.ignorados.push(clave);
                        continue;
                    }
                    // Okta repite `id` en el objeto (`{"id": …, "displayName": …}`): un atributo de solo lectura
                    // con su valor actual no es un cambio.
                    if solo_lectura(tipo, &clave.to_ascii_lowercase())
                        && doc
                            .as_object()
                            .and_then(|m| clave_real(m, &clave).and_then(|k| m.get(&k)))
                            == Some(&v)
                    {
                        continue;
                    }
                    let ruta =
                        RutaPatch::parse(&clave).map_err(|e| ScimError::invalid_path(e.0))?;
                    aplica_ruta(tipo, &mut doc, tipo_op, &ruta, Some(v), &mut resultado)?;
                }
            }
        }
    }
    resultado.doc = doc;
    Ok(resultado)
}

fn aplica_ruta(
    tipo: Tipo,
    doc: &mut Value,
    op: Operacion,
    ruta: &RutaPatch,
    valor: Option<Value>,
    resultado: &mut ResultadoPatch,
) -> ScimResult<()> {
    let RutaAtributo { uri, nombre, .. } = &ruta.atributo;
    if let Some(uri) = uri {
        if tipo == Tipo::Usuario && uri.eq_ignore_ascii_case(URN_ENTERPRISE) {
            resultado.ignorados.push(format!("{uri}:{nombre}"));
            return Ok(());
        } else if !uri.eq_ignore_ascii_case(tipo.urn()) {
            return Err(ScimError::invalid_path(format!(
                "Unknown schema '{uri}' in path"
            )));
        }
    }
    if solo_lectura(tipo, nombre) {
        return Err(ScimError::mutability(format!(
            "The attribute '{nombre}' is read-only"
        )));
    }
    if !guardado(tipo, nombre) {
        if conocido_no_guardado(tipo, nombre) {
            resultado.ignorados.push(nombre.clone());
            return Ok(());
        }
        return Err(ScimError::invalid_path(format!(
            "The attribute '{nombre}' is not defined in the {} schema",
            tipo.nombre()
        )));
    }
    if op != Operacion::Remove && valor.is_none() {
        return Err(ScimError::invalid_value(format!(
            "The operation on '{nombre}' requires a value"
        )));
    }
    // Quitar un atributo obligatorio (RFC 7644 §3.5.2.3).
    if op == Operacion::Remove
        && ruta.filtro.is_none()
        && ruta.sub.is_none()
        && matches!(
            (tipo, nombre.as_str()),
            (Tipo::Usuario, "username") | (Tipo::Grupo, "displayname")
        )
    {
        return Err(ScimError::mutability(format!(
            "The attribute '{nombre}' is required and cannot be removed"
        )));
    }
    let nombre_canonico = canonico(tipo, nombre);
    let Some(mapa) = doc.as_object_mut() else {
        return Err(ScimError::internal("Invalid resource representation"));
    };
    let clave = clave_real(mapa, nombre).unwrap_or_else(|| nombre_canonico.to_string());
    resultado.tocados.insert(nombre.clone());
    let multivalor = es_multivalor(tipo, nombre);

    match (&ruta.filtro, &ruta.sub) {
        // atributo
        (None, None) => {
            if multivalor {
                let actual = mapa.remove(&clave).map(como_lista).unwrap_or_default();
                let nueva = match op {
                    Operacion::Add => {
                        let mut lista = actual;
                        for e in como_lista(valor.unwrap_or(Value::Null)) {
                            if !lista.iter().any(|x| mismo_elemento(x, &e)) {
                                lista.push(e);
                            }
                        }
                        lista
                    }
                    Operacion::Replace => como_lista(valor.unwrap_or(Value::Null)),
                    Operacion::Remove => match valor {
                        // Forma de Entra: quitar los elementos que se indican.
                        Some(v) if !v.is_null() => {
                            let quitar = como_lista(v);
                            actual
                                .into_iter()
                                .filter(|x| !quitar.iter().any(|q| mismo_elemento(x, q)))
                                .collect()
                        }
                        _ => Vec::new(),
                    },
                };
                if !nueva.is_empty() {
                    mapa.insert(clave, Value::Array(nueva));
                }
            } else if op == Operacion::Remove {
                mapa.remove(&clave);
            } else if es_complejo(tipo, nombre) {
                let mut actual = mapa
                    .remove(&clave)
                    .unwrap_or_else(|| Value::Object(Map::new()));
                if !actual.is_object() {
                    actual = Value::Object(Map::new());
                }
                mezcla(&mut actual, valor.unwrap_or(Value::Null));
                mapa.insert(clave, actual);
            } else {
                mapa.insert(clave, valor.unwrap_or(Value::Null));
            }
        }
        // atributo.sub
        (None, Some(sub)) => {
            if multivalor {
                let mut lista = mapa.remove(&clave).map(como_lista).unwrap_or_default();
                if lista.is_empty() && op != Operacion::Remove {
                    let mut e = Map::new();
                    e.insert(sub_canonico(sub), valor.clone().unwrap_or(Value::Null));
                    lista.push(Value::Object(e));
                } else {
                    for e in lista.iter_mut() {
                        pon_sub(e, sub, op, valor.clone());
                    }
                }
                mapa.insert(clave, Value::Array(lista));
            } else if es_complejo(tipo, nombre) {
                let mut actual = mapa
                    .remove(&clave)
                    .unwrap_or_else(|| Value::Object(Map::new()));
                if !actual.is_object() {
                    actual = Value::Object(Map::new());
                }
                pon_sub(&mut actual, sub, op, valor);
                mapa.insert(clave, actual);
            } else {
                return Err(ScimError::invalid_path(format!(
                    "The attribute '{nombre}' has no sub-attributes"
                )));
            }
        }
        // atributo[filtro] y atributo[filtro].sub
        (Some(filtro), sub) => {
            if !multivalor {
                return Err(ScimError::invalid_path(format!(
                    "A value filter can only be used on a multi-valued attribute, not on '{nombre}'"
                )));
            }
            let mut lista = mapa.remove(&clave).map(como_lista).unwrap_or_default();
            let casan: Vec<usize> = lista
                .iter()
                .enumerate()
                .filter(|(_, e)| filtro.evalua(e))
                .map(|(i, _)| i)
                .collect();
            let restaura = |mapa: &mut Map<String, Value>, lista: Vec<Value>| {
                if !lista.is_empty() {
                    mapa.insert(clave.clone(), Value::Array(lista));
                }
            };
            if casan.is_empty() {
                if op == Operacion::Remove {
                    restaura(mapa, lista);
                    return Err(ScimError::no_target(format!(
                        "The value filter matched no '{nombre}' element"
                    )));
                }
                // add/replace sin coincidencias: se crea el elemento con lo que fija el filtro (Entra).
                let mut nuevo = Map::new();
                valores_de_filtro(filtro, &mut nuevo);
                let mut nuevo = Value::Object(nuevo);
                match sub {
                    Some(sub) => pon_sub(&mut nuevo, sub, Operacion::Replace, valor),
                    None => mezcla(&mut nuevo, valor.unwrap_or(Value::Null)),
                }
                lista.push(nuevo);
                restaura(mapa, lista);
                return Ok(());
            }
            if sub.is_some() && casan.len() > 1 && op != Operacion::Remove {
                restaura(mapa, lista);
                return Err(ScimError::invalid_value(format!(
                    "The value filter matched more than one '{nombre}' element"
                )));
            }
            match (op, sub) {
                (Operacion::Remove, None) => {
                    let mut i = 0;
                    lista.retain(|_| {
                        let quitar = casan.contains(&i);
                        i += 1;
                        !quitar
                    });
                }
                (_, Some(sub)) => {
                    for i in &casan {
                        pon_sub(&mut lista[*i], sub, op, valor.clone());
                    }
                }
                (_, None) => {
                    for i in &casan {
                        mezcla(&mut lista[*i], valor.clone().unwrap_or(Value::Null));
                    }
                }
            }
            restaura(mapa, lista);
        }
    }
    Ok(())
}

/// Nombre canónico de un subatributo (las rutas llegan en minúsculas).
fn sub_canonico(sub: &str) -> String {
    [
        "givenName",
        "familyName",
        "middleName",
        "formatted",
        "honorificPrefix",
        "honorificSuffix",
        "value",
        "type",
        "primary",
        "display",
    ]
    .iter()
    .find(|c| c.eq_ignore_ascii_case(sub))
    .map(|c| c.to_string())
    .unwrap_or_else(|| sub.to_string())
}

fn pon_sub(elemento: &mut Value, sub: &str, op: Operacion, valor: Option<Value>) {
    let Some(mapa) = elemento.as_object_mut() else {
        return;
    };
    let clave = clave_real(mapa, sub).unwrap_or_else(|| sub_canonico(sub));
    match op {
        Operacion::Remove => {
            mapa.remove(&clave);
        }
        _ => {
            mapa.insert(clave, valor.unwrap_or(Value::Null));
        }
    }
}

#[cfg(test)]
mod pruebas {
    use super::*;
    use serde_json::json;

    fn usuario() -> Value {
        json!({
            "schemas": [Tipo::Usuario.urn()],
            "id": "b",
            "userName": "ana@a.test",
            "displayName": "Ana",
            "name": {"formatted": "Ana"},
            "active": true,
            "emails": [{"value": "ana@a.test", "type": "work", "primary": true}],
            "locale": "en-US"
        })
    }

    fn patch(ops: Value) -> Value {
        json!({"schemas": [URN_PATCH], "Operations": ops})
    }

    #[test]
    fn estilo_entra() {
        let r = aplica(
            Tipo::Usuario,
            usuario(),
            &patch(json!([
                {"op": "Replace", "path": "active", "value": "False"},
                {"op": "Add", "path": "displayName", "value": "Ana P."},
                {"op": "Add", "path": "name.givenName", "value": "Ana"},
                {"op": "Replace", "path": "urn:ietf:params:scim:schemas:extension:enterprise:2.0:User:department", "value": "IT"},
                {"op": "Replace", "path": "title", "value": "CTO"},
                {"op": "Replace", "path": "emails[type eq \"work\"].value", "value": "ana.p@a.test"},
                {"op": "Add", "path": "externalId", "value": "ext-1"}
            ])),
        )
        .unwrap();
        assert_eq!(r.doc["active"], "False");
        assert_eq!(r.doc["displayName"], "Ana P.");
        assert_eq!(r.doc["name"]["givenName"], "Ana");
        assert_eq!(r.doc["emails"][0]["value"], "ana.p@a.test");
        assert_eq!(r.doc["externalId"], "ext-1");
        assert_eq!(r.ignorados.len(), 2);
        assert!(r.tocados.contains("emails") && r.tocados.contains("active"));
    }

    #[test]
    fn estilo_okta_sin_ruta() {
        let r = aplica(
            Tipo::Usuario,
            usuario(),
            &patch(
                json!([{"op": "replace", "value": {"active": false, "name.familyName": "Pérez"}}]),
            ),
        )
        .unwrap();
        assert_eq!(r.doc["active"], false);
        assert_eq!(r.doc["name"]["familyName"], "Pérez");
    }

    #[test]
    fn miembros_de_grupo() {
        let grupo = json!({"schemas": [Tipo::Grupo.urn()], "id": "g", "displayName": "Ventas",
                           "members": [{"value": "u1"}, {"value": "u2"}]});
        let r = aplica(
            Tipo::Grupo,
            grupo.clone(),
            &patch(json!([
                {"op": "Add", "path": "members", "value": [{"value": "u3"}, {"value": "u1"}]},
                {"op": "Remove", "path": "members", "value": [{"value": "u2"}]}
            ])),
        )
        .unwrap();
        let valores: Vec<&str> = r.doc["members"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["value"].as_str().unwrap())
            .collect();
        assert_eq!(valores, vec!["u1", "u3"]);
        let r = aplica(
            Tipo::Grupo,
            grupo.clone(),
            &patch(json!([{"op": "remove", "path": "members[value eq \"u1\"]"}])),
        )
        .unwrap();
        assert_eq!(r.doc["members"].as_array().unwrap().len(), 1);
        let r = aplica(
            Tipo::Grupo,
            grupo.clone(),
            &patch(json!([{"op": "remove", "path": "members"}])),
        )
        .unwrap();
        assert!(r.doc.get("members").is_none());
        // Okta: `replace` sin ruta repitiendo el id.
        let r = aplica(
            Tipo::Grupo,
            grupo,
            &patch(json!([{"op": "replace", "value": {"id": "g", "displayName": "Ventas EMEA"}}])),
        )
        .unwrap();
        assert_eq!(r.doc["displayName"], "Ventas EMEA");
        assert_eq!(r.doc["members"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn errores() {
        let casos = [
            (
                json!({"Operations": [{"op": "add", "path": "displayName", "value": "x"}]}),
                "invalidSyntax",
            ),
            (
                patch(json!([{"op": "move", "path": "displayName"}])),
                "invalidSyntax",
            ),
            (
                patch(json!([{"op": "replace", "path": "id", "value": "x"}])),
                "mutability",
            ),
            (
                patch(json!([{"op": "replace", "path": "groups", "value": []}])),
                "mutability",
            ),
            (
                patch(json!([{"op": "replace", "path": "dispalyName", "value": "x"}])),
                "invalidPath",
            ),
            (
                patch(json!([{"op": "replace", "path": "emails[type eq", "value": "x"}])),
                "invalidPath",
            ),
            (patch(json!([{"op": "remove"}])), "noTarget"),
            (
                patch(json!([{"op": "remove", "path": "emails[type eq \"home\"]"}])),
                "noTarget",
            ),
        ];
        for (cuerpo, esperado) in casos {
            let err = aplica(Tipo::Usuario, usuario(), &cuerpo).err().unwrap();
            assert_eq!(err.scim_type, Some(esperado), "{cuerpo}");
        }
        // Varios alias «work»: el filtro de Entra casa con más de uno.
        let mut u = usuario();
        u["emails"]
            .as_array_mut()
            .unwrap()
            .push(json!({"value": "ventas@a.test", "type": "work"}));
        let err = aplica(
            Tipo::Usuario,
            u,
            &patch(json!([{"op": "replace", "path": "emails[type eq \"work\"].value", "value": "x@a.test"}])),
        )
        .err()
        .unwrap();
        assert!(err.detail.contains("more than one 'emails' element"));
    }
}
