/*
 * SPDX-FileCopyrightText: 2026 bezenti
 *
 * SPDX-License-Identifier: AGPL-3.0-only
 */

//! Listas y búsquedas (`GET /Users`, `GET /Groups`, `POST …/.search`, `POST /.search`): filtro, orden,
//! paginación por índice y por cursor (RFC 9865) y proyección de atributos. Especificación §6 y §7.

use crate::{
    contexto::{Contexto, id_de_texto},
    error::{ScimError, ScimResult},
    esquema::{DEFAULT_PAGE, MAX_RESULTS, Tipo, URN_LIST, URN_SEARCH, filtrable},
    filtro::{Filtro, RutaAtributo, busca, busca_sub},
    recursos::{Miembros, campo, representa},
};
use registry::schema::structs::Account;
use serde_json::{Map, Value, json};
use std::{cmp::Ordering, collections::BTreeSet};
use trc::ScimEvent;

/// Máximo de candidatos que se evalúan en memoria (especificación §6).
pub const MAX_CANDIDATOS: u64 = 5000;

#[derive(Debug, Default)]
pub struct Parametros {
    pub filtro: Option<Filtro>,
    pub filtro_texto: Option<String>,
    pub start_index: Option<usize>,
    pub count: Option<usize>,
    pub cursor: Option<String>,
    pub sort_by: Option<RutaAtributo>,
    pub sort_texto: Option<String>,
    pub descendente: bool,
    pub atributos: Vec<String>,
    pub excluidos: Vec<String>,
}

fn lista_atributos(texto: &str) -> Vec<String> {
    texto
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn numero(nombre: &str, texto: &str) -> ScimResult<i64> {
    texto.trim().parse::<i64>().map_err(|_| {
        ScimError::invalid_value(format!("The parameter '{nombre}' must be an integer"))
    })
}

impl Parametros {
    fn pon(&mut self, clave: &str, valor: &str) -> ScimResult<()> {
        match clave.to_ascii_lowercase().as_str() {
            "filter" => {
                self.filtro =
                    Some(Filtro::parse(valor).map_err(|e| ScimError::invalid_filter(e.0))?);
                self.filtro_texto = Some(valor.to_string());
            }
            "startindex" => self.start_index = Some(numero("startIndex", valor)?.max(1) as usize),
            "count" => self.count = Some(numero("count", valor)?.max(0) as usize),
            "cursor" => self.cursor = Some(valor.to_string()),
            "sortby" => {
                self.sort_by =
                    Some(RutaAtributo::parse(valor).map_err(|e| {
                        ScimError::invalid_value(format!("Invalid sortBy: {}", e.0))
                    })?);
                self.sort_texto = Some(valor.to_string());
            }
            "sortorder" => {
                self.descendente = match valor.to_ascii_lowercase().as_str() {
                    "ascending" => false,
                    "descending" => true,
                    _ => {
                        return Err(ScimError::invalid_value(
                            "sortOrder must be 'ascending' or 'descending'",
                        ));
                    }
                }
            }
            "attributes" => self.atributos = lista_atributos(valor),
            "excludedattributes" => self.excluidos = lista_atributos(valor),
            _ => {}
        }
        Ok(())
    }

    /// Parámetros de la cadena de consulta.
    pub fn de_query(query: Option<&str>) -> ScimResult<Self> {
        let mut p = Parametros::default();
        if let Some(query) = query {
            for (k, v) in http_proto::form_urlencoded::parse(query.as_bytes()) {
                p.pon(&k, &v)?;
            }
        }
        Ok(p)
    }

    /// Parámetros de un `SearchRequest` (`POST …/.search`).
    pub fn de_cuerpo(cuerpo: &Value) -> ScimResult<Self> {
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
                        .is_some_and(|u| u.eq_ignore_ascii_case(URN_SEARCH))
                })
            });
        if !schemas_ok {
            return Err(ScimError::invalid_syntax(format!(
                "The 'schemas' attribute must be [\"{URN_SEARCH}\"]"
            )));
        }
        let mut p = Parametros::default();
        for (k, v) in mapa {
            if k.eq_ignore_ascii_case("schemas") {
                continue;
            }
            let texto = match v {
                Value::String(s) => s.clone(),
                Value::Number(n) => n.to_string(),
                Value::Array(a) => a
                    .iter()
                    .filter_map(|x| x.as_str())
                    .collect::<Vec<_>>()
                    .join(","),
                Value::Null => continue,
                _ => {
                    return Err(ScimError::invalid_value(format!("Invalid value for '{k}'")));
                }
            };
            p.pon(k, &texto)?;
        }
        Ok(p)
    }

    fn huella(&self, tipos: &[Tipo]) -> u64 {
        let texto = format!(
            "{:?}|{}|{}|{}",
            tipos,
            self.filtro_texto.as_deref().unwrap_or_default(),
            self.sort_texto.as_deref().unwrap_or_default(),
            self.descendente
        );
        xxhash_rust::xxh3::xxh3_64(texto.as_bytes())
    }
}

/// Candidatos de un tipo: por índice si el filtro tiene un `eq` indexado; si no, todas las cuentas visibles.
/// Devuelve también si hace falta evaluar el filtro en memoria.
async fn candidatos(
    ctx: &Contexto<'_>,
    tipo: Tipo,
    filtro: Option<&Filtro>,
) -> ScimResult<Vec<u32>> {
    let visibles = ctx.ids_visibles(tipo).await?;
    let Some(filtro) = filtro else {
        return Ok(visibles.into_iter().collect());
    };
    let mut indexado: Option<BTreeSet<u32>> = None;
    for (clave, valor) in filtro.igualdades() {
        let Some(valor) = valor.as_str() else {
            continue;
        };
        let encontrados: Option<BTreeSet<u32>> = match (tipo, clave.as_str()) {
            (_, "id") => Some(id_de_texto(valor).into_iter().collect()),
            (Tipo::Usuario, "username" | "emails" | "emails.value") => {
                Some(ctx.id_de_direccion(valor).await?.into_iter().collect())
            }
            (_, "externalid") => Some(ctx.ids_external_id(valor).await?.into_iter().collect()),
            (Tipo::Usuario, "groups" | "groups.value") => match id_de_texto(valor) {
                Some(g) => Some(ctx.ids_miembros(g).await?.into_iter().collect()),
                None => Some(BTreeSet::new()),
            },
            (Tipo::Grupo, "members" | "members.value") => match id_de_texto(valor) {
                Some(u) => match ctx.cuenta(u, Some(Tipo::Usuario)).await? {
                    Some(usuario) => match &usuario.cuenta {
                        Account::User(u) => {
                            Some(u.member_group_ids.iter().map(|g| g.document_id()).collect())
                        }
                        _ => Some(BTreeSet::new()),
                    },
                    None => Some(BTreeSet::new()),
                },
                None => Some(BTreeSet::new()),
            },
            _ => None,
        };
        if let Some(encontrados) = encontrados {
            indexado = Some(match indexado {
                Some(previo) => previo.intersection(&encontrados).copied().collect(),
                None => encontrados,
            });
        }
    }
    match indexado {
        Some(ids) => Ok(ids
            .into_iter()
            .filter(|id| visibles.contains(*id))
            .collect()),
        None => {
            if visibles.len() > MAX_CANDIDATOS {
                return Err(ScimError::too_many(format!(
                    "The filter matches more than {MAX_CANDIDATOS} candidates before it can be evaluated; \
                     add an indexed condition (id, userName, emails, externalId, groups or members) with 'and'"
                )));
            }
            Ok(visibles.into_iter().collect())
        }
    }
}

/// Valor por el que se ordena (cadena en minúsculas, número o booleano).
fn clave_orden(doc: &Value, ruta: &RutaAtributo) -> Option<Value> {
    let v = busca(doc, ruta)?;
    let v = match (&ruta.sub, v) {
        (Some(sub), Value::Array(lista)) => lista
            .iter()
            .find(|e| busca_sub(e, "primary").and_then(|p| p.as_bool()) == Some(true))
            .or_else(|| lista.first())
            .and_then(|e| busca_sub(e, sub))?,
        (Some(sub), v) => busca_sub(v, sub)?,
        (None, Value::Array(lista)) => lista
            .first()
            .and_then(|e| busca_sub(e, "value").or(Some(e)))?,
        (None, v) => v,
    };
    match v {
        Value::String(s) => Some(Value::from(s.to_lowercase())),
        Value::Null => None,
        v => Some(v.clone()),
    }
}

fn compara_orden(a: &Option<Value>, b: &Option<Value>) -> Ordering {
    match (a, b) {
        (None, None) => Ordering::Equal,
        // Sin valor, al final (RFC 7644 §3.4.2.3).
        (None, Some(_)) => Ordering::Greater,
        (Some(_), None) => Ordering::Less,
        (Some(Value::String(x)), Some(Value::String(y))) => x.cmp(y),
        (Some(Value::Number(x)), Some(Value::Number(y))) => x
            .as_f64()
            .partial_cmp(&y.as_f64())
            .unwrap_or(Ordering::Equal),
        (Some(Value::Bool(x)), Some(Value::Bool(y))) => x.cmp(y),
        (Some(x), Some(y)) => x.to_string().cmp(&y.to_string()),
    }
}

/// `attributes` / `excludedAttributes` (RFC 7644 §3.4.2.5). `id`, `schemas` y `meta.resourceType` siempre.
pub fn proyecta(tipo: Tipo, doc: Value, atributos: &[String], excluidos: &[String]) -> Value {
    if atributos.is_empty() && excluidos.is_empty() {
        return doc;
    }
    let Value::Object(mut mapa) = doc else {
        return doc;
    };
    let prefijo = format!("{}:", tipo.urn().to_ascii_lowercase());
    let normaliza = |s: &String| -> (String, Option<String>) {
        let s = s.to_ascii_lowercase();
        let s = s.strip_prefix(&prefijo).map(|x| x.to_string()).unwrap_or(s);
        match s.split_once('.') {
            Some((a, b)) => (a.to_string(), Some(b.to_string())),
            None => (s, None),
        }
    };
    let clave_de = |mapa: &Map<String, Value>, nombre: &str| {
        mapa.keys()
            .find(|k| k.eq_ignore_ascii_case(nombre))
            .cloned()
    };
    if !atributos.is_empty() {
        let pedidos: Vec<(String, Option<String>)> = atributos.iter().map(normaliza).collect();
        let mut salida = Map::new();
        for (k, v) in mapa {
            let minus = k.to_ascii_lowercase();
            if minus == "id" || minus == "schemas" {
                salida.insert(k, v);
                continue;
            }
            let subs: Vec<&Option<String>> = pedidos
                .iter()
                .filter(|(a, _)| *a == minus)
                .map(|(_, s)| s)
                .collect();
            if subs.is_empty() {
                if minus == "meta" {
                    salida.insert(k, json!({"resourceType": v.get("resourceType").cloned().unwrap_or(Value::Null)}));
                }
                continue;
            }
            if subs.iter().any(|s| s.is_none()) {
                salida.insert(k, v);
                continue;
            }
            let quedan: Vec<&str> = subs.iter().filter_map(|s| s.as_deref()).collect();
            let filtra = |e: &Value| -> Value {
                match e {
                    Value::Object(o) => Value::Object(
                        o.iter()
                            .filter(|(k, _)| quedan.iter().any(|q| k.eq_ignore_ascii_case(q)))
                            .map(|(k, v)| (k.clone(), v.clone()))
                            .collect(),
                    ),
                    otro => otro.clone(),
                }
            };
            let v = match &v {
                Value::Array(lista) => Value::Array(lista.iter().map(filtra).collect()),
                otro => filtra(otro),
            };
            salida.insert(k, v);
        }
        return Value::Object(salida);
    }
    for (a, sub) in excluidos.iter().map(normaliza) {
        if a == "id" || a == "schemas" {
            continue;
        }
        let Some(clave) = clave_de(&mapa, &a) else {
            continue;
        };
        match sub {
            None => {
                mapa.remove(&clave);
            }
            Some(sub) => {
                let quita = |e: &mut Value| {
                    if let Some(o) = e.as_object_mut()
                        && let Some(k) = o.keys().find(|k| k.eq_ignore_ascii_case(&sub)).cloned()
                    {
                        o.remove(&k);
                    }
                };
                if let Some(v) = mapa.get_mut(&clave) {
                    match v {
                        Value::Array(lista) => lista.iter_mut().for_each(quita),
                        otro => quita(otro),
                    }
                }
            }
        }
    }
    Value::Object(mapa)
}

/// ¿Pide la proyección los miembros de un grupo?
pub fn incluye_miembros(atributos: &[String], excluidos: &[String]) -> bool {
    let menciona = |l: &[String]| {
        l.iter().any(|a| {
            let a = a.to_ascii_lowercase();
            a == "members" || a.starts_with("members.") || a.ends_with(":members")
        })
    };
    if !atributos.is_empty() {
        menciona(atributos)
    } else {
        !menciona(excluidos)
    }
}

/// Lista o búsqueda sobre uno o varios tipos de recurso.
pub async fn lista(ctx: &Contexto<'_>, tipos: &[Tipo], p: Parametros) -> ScimResult<Value> {
    // Validación de rutas de filtro y orden.
    if let Some(filtro) = &p.filtro {
        for ruta in filtro.rutas() {
            if let Some(uri) = &ruta.uri
                && !tipos.iter().any(|t| uri.eq_ignore_ascii_case(t.urn()))
            {
                return Err(ScimError::invalid_filter(format!(
                    "The schema '{uri}' cannot be used in a filter"
                )));
            }
            let clave = ruta.clave();
            if !tipos.iter().any(|t| filtrable(*t, &clave)) {
                return Err(ScimError::invalid_filter(format!(
                    "The attribute '{clave}' cannot be used in a filter"
                )));
            }
        }
    }
    if let Some(sort) = &p.sort_by
        && !tipos.iter().any(|t| filtrable(*t, &sort.clave()))
    {
        return Err(ScimError::invalid_value(format!(
            "Sorting by '{}' is not supported",
            sort.clave()
        )));
    }
    if p.cursor.is_some() && p.start_index.is_some() {
        return Err(ScimError::invalid_value(
            "The parameters 'cursor' and 'startIndex' cannot be combined",
        ));
    }
    let count = p.count.unwrap_or(DEFAULT_PAGE).min(MAX_RESULTS);
    let huella = p.huella(tipos);
    let inicio = match &p.cursor {
        None => p.start_index.unwrap_or(1) - 1,
        Some(c) if c.is_empty() => 0,
        Some(c) => {
            let (pos, h) = c.split_once('.').ok_or_else(ScimError::invalid_cursor)?;
            let h = u64::from_str_radix(h, 16).map_err(|_| ScimError::invalid_cursor())?;
            if h != huella {
                return Err(ScimError::invalid_cursor());
            }
            usize::from_str_radix(pos, 16).map_err(|_| ScimError::invalid_cursor())?
        }
    };
    let quiere_miembros = incluye_miembros(&p.atributos, &p.excluidos);
    let ordena_por_id = p
        .sort_by
        .as_ref()
        .is_none_or(|s| s.clave() == "id" && s.uri.is_none());

    // (tipo, id) candidatos.
    let mut todos: Vec<(Tipo, u32)> = Vec::new();
    for tipo in tipos {
        for id in candidatos(ctx, *tipo, p.filtro.as_ref()).await? {
            todos.push((*tipo, id));
        }
    }

    let (total, pagina): (usize, Vec<Value>) = if p.filtro.is_none() && ordena_por_id {
        // Camino rápido: sin filtro y por id, solo se leen las cuentas de la página.
        if p.descendente {
            todos.reverse();
        }
        let total = todos.len();
        let mut pagina = Vec::new();
        for (tipo, id) in todos.into_iter().skip(inicio).take(count) {
            if let Some(leida) = ctx.cuenta(id, Some(tipo)).await? {
                let doc = representa(
                    ctx,
                    &leida,
                    if quiere_miembros {
                        Miembros::Limitados
                    } else {
                        Miembros::Excluidos
                    },
                )
                .await?;
                pagina.push(proyecta(tipo, doc, &p.atributos, &p.excluidos));
            }
        }
        (total, pagina)
    } else {
        let mut encontrados: Vec<(Tipo, u32, Value)> = Vec::new();
        for (tipo, id) in todos {
            if let Some(leida) = ctx.cuenta(id, Some(tipo)).await? {
                let doc = representa(ctx, &leida, Miembros::Todos).await?;
                if p.filtro.as_ref().is_none_or(|f| f.evalua(&doc)) {
                    encontrados.push((tipo, id, doc));
                }
            }
        }
        if let Some(sort) = p.sort_by.as_ref().filter(|_| !ordena_por_id) {
            let mut claves: Vec<(Option<Value>, (Tipo, u32, Value))> = encontrados
                .into_iter()
                .map(|e| (clave_orden(&e.2, sort), e))
                .collect();
            claves.sort_by(|a, b| {
                let o = compara_orden(&a.0, &b.0);
                if p.descendente && a.0.is_some() && b.0.is_some() {
                    o.reverse()
                } else {
                    o
                }
            });
            encontrados = claves.into_iter().map(|(_, e)| e).collect();
        } else if p.descendente {
            encontrados.reverse();
        }
        let total = encontrados.len();
        let mut pagina = Vec::new();
        for (tipo, _, mut doc) in encontrados.into_iter().skip(inicio).take(count) {
            if tipo == Tipo::Grupo {
                let n = doc
                    .get("members")
                    .and_then(|m| m.as_array())
                    .map_or(0, |m| m.len());
                if !quiere_miembros {
                    if let Some(m) = doc.as_object_mut() {
                        m.remove("members");
                    }
                } else if n > crate::recursos::MAX_MIEMBROS {
                    return Err(ScimError::too_many(format!(
                        "A group in the results has more than {} members; use excludedAttributes=members",
                        crate::recursos::MAX_MIEMBROS
                    )));
                }
            }
            pagina.push(proyecta(tipo, doc, &p.atributos, &p.excluidos));
        }
        (total, pagina)
    };

    trc::event!(
        Scim(ScimEvent::Query),
        Type = tipos
            .iter()
            .map(|t| t.nombre())
            .collect::<Vec<_>>()
            .join(","),
        Details = p.filtro_texto.clone().unwrap_or_default(),
        Total = total as u64,
        Size = pagina.len() as u64,
        SpanId = ctx.session.session_id,
    );

    let mut respuesta = json!({
        "schemas": [URN_LIST],
        "totalResults": total,
        "itemsPerPage": pagina.len(),
    });
    if p.cursor.is_some() {
        let siguiente = inicio + pagina.len();
        if siguiente < total && count > 0 {
            respuesta["nextCursor"] = Value::from(format!("{siguiente:x}.{huella:x}"));
        }
        if inicio > 0 {
            let anterior = inicio.saturating_sub(count.max(1));
            respuesta["previousCursor"] = Value::from(format!("{anterior:x}.{huella:x}"));
        }
    } else {
        respuesta["startIndex"] = Value::from(inicio + 1);
    }
    respuesta["Resources"] = Value::Array(pagina);
    Ok(respuesta)
}

#[cfg(test)]
mod pruebas {
    use super::*;

    #[test]
    fn proyeccion() {
        let doc = json!({"schemas": ["s"], "id": "1", "userName": "a@b", "displayName": "A",
                         "emails": [{"value": "a@b", "type": "work", "primary": true}],
                         "meta": {"resourceType": "User", "version": "v"}});
        let p = proyecta(
            Tipo::Usuario,
            doc.clone(),
            &["userName".into(), "emails.value".into()],
            &[],
        );
        assert_eq!(
            p,
            json!({"schemas": ["s"], "id": "1", "userName": "a@b",
                             "emails": [{"value": "a@b"}], "meta": {"resourceType": "User"}})
        );
        let p = proyecta(
            Tipo::Usuario,
            doc,
            &[],
            &["emails".into(), "id".into(), "meta.version".into()],
        );
        assert!(
            p.get("emails").is_none()
                && p.get("id").is_some()
                && p["meta"].get("version").is_none()
        );
    }

    #[test]
    fn parametros() {
        let p = Parametros::de_query(Some(
            "filter=userName%20eq%20%22a%40b%22&startIndex=0&count=500&sortOrder=descending",
        ))
        .unwrap();
        assert!(
            p.filtro.is_some() && p.start_index == Some(1) && p.count == Some(500) && p.descendente
        );
        assert_eq!(
            Parametros::de_query(Some("filter=userName%20zz%20%22a%22"))
                .unwrap_err()
                .scim_type,
            Some("invalidFilter")
        );
        assert!(incluye_miembros(&[], &[]));
        assert!(!incluye_miembros(&[], &["members".into()]));
        assert!(!incluye_miembros(&["displayName".into()], &[]));
    }
}
