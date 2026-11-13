/*
 * SPDX-FileCopyrightText: 2026 bezenti
 *
 * SPDX-License-Identifier: AGPL-3.0-only
 */

//! User y Group ↔ cuentas de Stalwart: representación SCIM, validación de la entrada y escritura (crear y
//! reemplazar). El PATCH se reduce a un reemplazo (ver `patch.rs`). Especificación: `bezenti/especificaciones/
//! scim.md`, §5.

use crate::{
    contexto::{Contexto, CuentaLeida, id_de_texto, id_texto},
    error::{ScimError, ScimResult},
    esquema::{
        Tipo, URN_ENTERPRISE, atributo_empresa, conocido_no_guardado, guardado, uri_conocida,
    },
};
use registry::{
    schema::{
        enums::{Locale, Permission, TimeZone},
        structs::{Account, Permissions},
    },
    types::{EnumImpl, datetime::UTCDateTime},
};
use serde_json::{Map, Value, json};
use std::collections::{BTreeSet, HashSet};
use trc::ScimEvent;

/// Máximo de miembros que se devuelven al leer un grupo (especificación §5.2).
pub const MAX_MIEMBROS: usize = 1000;
/// Longitud máxima de la parte local derivada del nombre de un grupo.
const MAX_LOCAL: usize = 64;

// ---------------------------------------------------------------------------------------------------------------
// Representación

/// Huella del recurso para `meta.version` y `ETag` (sin la propia versión).
pub fn huella(doc: &Value) -> String {
    let mut copia = doc.clone();
    if let Some(meta) = copia.get_mut("meta").and_then(|m| m.as_object_mut()) {
        meta.remove("version");
    }
    format!(
        "W/\"{:016x}\"",
        xxhash_rust::xxh3::xxh3_64(copia.to_string().as_bytes())
    )
}

fn con_meta(
    ctx: &Contexto<'_>,
    tipo: Tipo,
    id: u32,
    creado: &UTCDateTime,
    mut doc: Value,
) -> Value {
    let mut meta = json!({
        "resourceType": tipo.nombre(),
        "location": format!("{}/{}/{}", ctx.base, tipo.endpoint(), id_texto(id)),
    });
    if creado.is_valid() && creado.timestamp() > 0 {
        meta["created"] = Value::from(creado.to_string());
    }
    doc["meta"] = meta;
    let version = huella(&doc);
    doc["meta"]["version"] = Value::from(version);
    doc
}

/// Qué hacer con los miembros de un grupo al representarlo.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Miembros {
    /// Incluirlos, con el límite de [`MAX_MIEMBROS`] (salida normal).
    Limitados,
    /// No incluirlos en la salida (la versión los sigue teniendo en cuenta).
    Excluidos,
    /// Incluirlos todos (uso interno: filtros y PATCH).
    Todos,
}

/// Representación SCIM de una cuenta visible.
pub async fn representa(
    ctx: &Contexto<'_>,
    leida: &CuentaLeida,
    miembros: Miembros,
) -> ScimResult<Value> {
    match &leida.cuenta {
        Account::User(usuario) => {
            let dominio = ctx
                .nombre_dominio_cualquiera(usuario.domain_id.document_id())
                .await?;
            let principal = format!("{}@{}", usuario.name, dominio);
            let mut doc = json!({
                "schemas": [Tipo::Usuario.urn()],
                "id": id_texto(leida.id),
                "userName": principal,
            });
            if let Some(external_id) = &usuario.external_id {
                doc["externalId"] = Value::from(external_id.as_str());
            }
            if let Some(descripcion) = usuario.description.as_deref().filter(|d| !d.is_empty()) {
                doc["displayName"] = Value::from(descripcion);
                doc["name"] = json!({ "formatted": descripcion });
            }
            doc["active"] = Value::Bool(ctx.puede_autenticarse(&leida.cuenta).await?);
            let mut emails = vec![json!({"value": principal, "type": "work", "primary": true})];
            for alias in usuario.aliases.values() {
                let dominio = ctx
                    .nombre_dominio_cualquiera(alias.domain_id.document_id())
                    .await?;
                emails
                    .push(json!({"value": format!("{}@{}", alias.name, dominio), "type": "work"}));
            }
            doc["emails"] = Value::Array(emails);
            // Siempre tiene valor (por defecto en-US) y se devuelve con los dos nombres (documentación pública).
            let locale = usuario.locale.as_str();
            doc["locale"] = Value::from(locale);
            doc["preferredLanguage"] = Value::from(locale);
            if let Some(zona) = &usuario.time_zone {
                doc["timezone"] = Value::from(zona.as_str());
            }
            let mut grupos = Vec::new();
            for gid in usuario.member_group_ids.iter() {
                let gid = gid.document_id();
                if let Some(grupo) = ctx.cuenta(gid, Some(Tipo::Grupo)).await? {
                    grupos.push(json!({
                        "value": id_texto(gid),
                        "$ref": format!("{}/Groups/{}", ctx.base, id_texto(gid)),
                        "display": nombre_visible(&grupo),
                        "type": "direct",
                    }));
                }
            }
            if !grupos.is_empty() {
                doc["groups"] = Value::Array(grupos);
            }
            Ok(con_meta(
                ctx,
                Tipo::Usuario,
                leida.id,
                &usuario.created_at,
                doc,
            ))
        }
        Account::Group(grupo) => {
            let mut doc = json!({
                "schemas": [Tipo::Grupo.urn()],
                "id": id_texto(leida.id),
                "displayName": nombre_visible(leida),
            });
            if let Some(external_id) = &grupo.external_id {
                doc["externalId"] = Value::from(external_id.as_str());
            }
            let lista = miembros_visibles(ctx, leida.id).await?;
            if miembros == Miembros::Limitados && lista.len() > MAX_MIEMBROS {
                return Err(ScimError::too_many(format!(
                    "The group has more than {MAX_MIEMBROS} members; request it with excludedAttributes=members"
                )));
            }
            let mut out = Vec::with_capacity(lista.len());
            for miembro in &lista {
                out.push(json!({
                    "value": id_texto(miembro.id),
                    "$ref": format!("{}/Users/{}", ctx.base, id_texto(miembro.id)),
                    "display": nombre_visible(miembro),
                    "type": "User",
                }));
            }
            if !out.is_empty() {
                doc["members"] = Value::Array(out);
            }
            let mut doc = con_meta(ctx, Tipo::Grupo, leida.id, &grupo.created_at, doc);
            if miembros == Miembros::Excluidos
                && let Some(map) = doc.as_object_mut()
            {
                // La versión ya incluye a los miembros; solo se omiten en la salida.
                map.remove("members");
            }
            Ok(doc)
        }
    }
}

/// Nombre visible: la descripción o, si no hay, la dirección.
pub fn nombre_visible(leida: &CuentaLeida) -> String {
    match &leida.cuenta {
        Account::User(u) => u
            .description
            .clone()
            .filter(|d| !d.is_empty())
            .unwrap_or_else(|| u.name.clone()),
        Account::Group(g) => g
            .description
            .clone()
            .filter(|d| !d.is_empty())
            .unwrap_or_else(|| g.name.clone()),
    }
}

/// Usuarios visibles miembros de un grupo, por id.
pub async fn miembros_visibles(ctx: &Contexto<'_>, grupo: u32) -> ScimResult<Vec<CuentaLeida>> {
    let mut out = Vec::new();
    for id in ctx.ids_miembros(grupo).await? {
        if let Some(usuario) = ctx.cuenta(id, Some(Tipo::Usuario)).await? {
            out.push(usuario);
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------------------------------------------
// Validación de la entrada

/// Busca un atributo de primer nivel sin distinguir mayúsculas.
pub fn campo<'x>(doc: &'x Map<String, Value>, nombre: &str) -> Option<&'x Value> {
    doc.iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(nombre))
        .map(|(_, v)| v)
        .filter(|v| !v.is_null())
}

/// Comprueba `schemas` y que cada atributo sea de un esquema conocido (doc: «Schema handling»). Los atributos con
/// el prefijo URI del esquema principal se pasan a su nombre corto.
pub fn valida_atributos(tipo: Tipo, doc: &Value) -> ScimResult<Map<String, Value>> {
    let Some(mapa) = doc.as_object() else {
        return Err(ScimError::invalid_syntax(
            "The request body must be a JSON object",
        ));
    };
    let Some(schemas) = campo(mapa, "schemas").and_then(|s| s.as_array()) else {
        return Err(ScimError::invalid_syntax(
            "The 'schemas' attribute is missing",
        ));
    };
    if !schemas
        .iter()
        .filter_map(|s| s.as_str())
        .any(|s| s.eq_ignore_ascii_case(tipo.urn()))
    {
        return Err(ScimError::invalid_syntax(format!(
            "The 'schemas' attribute must include {}",
            tipo.urn()
        )));
    }
    for uri in schemas {
        match uri.as_str() {
            Some(uri) if uri_conocida(tipo, uri) => {}
            _ => {
                return Err(ScimError::invalid_syntax(format!(
                    "Unknown schema {uri} for a {}",
                    tipo.nombre()
                )));
            }
        }
    }

    let mut salida = Map::new();
    let mut vistos = HashSet::new();
    let prefijo = format!("{}:", tipo.urn().to_ascii_lowercase());
    for (clave, valor) in mapa {
        let minus = clave.to_ascii_lowercase();
        let nombre = minus.strip_prefix(&prefijo).unwrap_or(&minus).to_string();
        if !vistos.insert(nombre.clone()) {
            return Err(ScimError::invalid_syntax(format!(
                "The attribute '{clave}' appears more than once"
            )));
        }
        if nombre == "schemas" {
            continue;
        }
        if guardado(tipo, &nombre) {
            salida.insert(nombre, valor.clone());
        } else if conocido_no_guardado(tipo, &nombre) {
            continue;
        } else if tipo == Tipo::Usuario && minus == URN_ENTERPRISE.to_ascii_lowercase() {
            // Extensión empresarial: se acepta y se descarta, pero sus atributos deben ser los del RFC.
            if let Some(ext) = valor.as_object() {
                for k in ext.keys() {
                    if !atributo_empresa(&k.to_ascii_lowercase()) {
                        return Err(ScimError::invalid_syntax(format!(
                            "The attribute '{k}' is not defined in {URN_ENTERPRISE}"
                        )));
                    }
                }
            }
        } else if minus.starts_with("urn:") {
            return Err(ScimError::invalid_syntax(format!(
                "Unknown schema extension '{clave}'"
            )));
        } else {
            return Err(ScimError::invalid_syntax(format!(
                "The attribute '{clave}' is not defined in the {} schema",
                tipo.nombre()
            )));
        }
    }
    Ok(salida)
}

fn texto(doc: &Map<String, Value>, nombre: &str) -> ScimResult<Option<String>> {
    match campo(doc, nombre) {
        None => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.trim().to_string()).filter(|s| !s.is_empty())),
        Some(_) => Err(ScimError::invalid_value(format!(
            "The attribute '{nombre}' must be a string"
        ))),
    }
}

/// `active` como booleano o como cadena «true»/«false» (Entra manda «False»).
pub fn booleano(v: &Value) -> Option<bool> {
    match v {
        Value::Bool(b) => Some(*b),
        Value::String(s) if s.eq_ignore_ascii_case("true") => Some(true),
        Value::String(s) if s.eq_ignore_ascii_case("false") => Some(false),
        _ => None,
    }
}

/// Normaliza un locale SCIM (`en-US`, `EN-us`, `en_US`, `ca-ES@valencia`) al del registro.
pub fn normaliza_locale(texto: &str) -> Option<Locale> {
    let base = texto.split('@').next().unwrap_or(texto).replace('_', "-");
    if let Some(locale) = Locale::parse(&base) {
        return Some(locale);
    }
    (0..Locale::COUNT as u16)
        .filter_map(Locale::from_id)
        .find(|l| l.as_str().eq_ignore_ascii_case(&base))
}

pub fn normaliza_zona(texto: &str) -> Option<TimeZone> {
    TimeZone::parse(texto).or_else(|| {
        (0..TimeZone::COUNT as u16)
            .filter_map(TimeZone::from_id)
            .find(|z| z.as_str().eq_ignore_ascii_case(texto))
    })
}

/// Lo que dice una petición sobre un usuario (POST, PUT o el resultado de un PATCH).
#[derive(Debug, Default)]
pub struct EntradaUsuario {
    pub user_name: String,
    pub external_id: Option<String>,
    pub descripcion: Option<String>,
    pub activo: bool,
    pub alias: Vec<String>,
    pub locale: Locale,
    pub zona: Option<TimeZone>,
}

impl EntradaUsuario {
    /// `doc`: atributos ya validados por [`valida_atributos`] (nombres en minúsculas).
    pub fn de(doc: &Map<String, Value>) -> ScimResult<Self> {
        let Some(user_name) = texto(doc, "username")? else {
            return Err(ScimError::invalid_value(
                "The attribute 'userName' is required",
            ));
        };
        let nombre = match campo(doc, "name") {
            None => None,
            Some(Value::Object(n)) => Some(n),
            Some(_) => {
                return Err(ScimError::invalid_value(
                    "The attribute 'name' must be an object",
                ));
            }
        };
        let descripcion = match texto(doc, "displayname")? {
            Some(d) => Some(d),
            None => match nombre {
                Some(n) => match texto(n, "formatted")? {
                    Some(f) => Some(f),
                    None => {
                        let partes: Vec<String> = [texto(n, "givenName")?, texto(n, "familyName")?]
                            .into_iter()
                            .flatten()
                            .collect();
                        (!partes.is_empty()).then(|| partes.join(" "))
                    }
                },
                None => None,
            },
        };
        let activo = match campo(doc, "active") {
            None => true,
            Some(v) => booleano(v).ok_or_else(|| {
                ScimError::invalid_value("The attribute 'active' must be a boolean")
            })?,
        };
        let mut alias = Vec::new();
        let mut vistos = BTreeSet::new();
        vistos.insert(user_name.to_lowercase());
        match campo(doc, "emails") {
            None => {}
            Some(Value::Array(lista)) => {
                for email in lista {
                    let valor = match email {
                        Value::Object(e) => texto(e, "value")?,
                        Value::String(s) => Some(s.trim().to_string()),
                        _ => None,
                    };
                    let Some(valor) = valor else {
                        return Err(ScimError::invalid_value(
                            "Each element of 'emails' must have a 'value'",
                        ));
                    };
                    if vistos.insert(valor.to_lowercase()) {
                        alias.push(valor.to_lowercase());
                    }
                }
            }
            Some(_) => {
                return Err(ScimError::invalid_value(
                    "The attribute 'emails' must be a list",
                ));
            }
        }
        let locale = match texto(doc, "locale")?.or(texto(doc, "preferredlanguage")?) {
            None => Locale::EnUS,
            Some(l) => normaliza_locale(&l).ok_or_else(|| {
                ScimError::invalid_value(format!("The locale '{l}' is not supported"))
            })?,
        };
        let zona = match texto(doc, "timezone")? {
            None => None,
            Some(z) => Some(normaliza_zona(&z).ok_or_else(|| {
                ScimError::invalid_value(format!(
                    "The time zone '{z}' is not a known IANA identifier"
                ))
            })?),
        };
        Ok(EntradaUsuario {
            user_name: user_name.to_lowercase(),
            external_id: texto(doc, "externalid")?,
            descripcion,
            activo,
            alias,
            locale,
            zona,
        })
    }
}

#[derive(Debug, Default)]
pub struct EntradaGrupo {
    pub descripcion: String,
    pub external_id: Option<String>,
    pub miembros: Vec<String>,
}

impl EntradaGrupo {
    pub fn de(doc: &Map<String, Value>) -> ScimResult<Self> {
        let Some(descripcion) = texto(doc, "displayname")? else {
            return Err(ScimError::invalid_value(
                "The attribute 'displayName' is required",
            ));
        };
        let mut miembros = Vec::new();
        match campo(doc, "members") {
            None => {}
            Some(Value::Array(lista)) => {
                for m in lista {
                    let valor = match m {
                        Value::Object(e) => {
                            if let Some(t) = texto(e, "type")?
                                && !t.eq_ignore_ascii_case("user")
                            {
                                return Err(ScimError::invalid_value(
                                    "Only users can be members of a group (groups cannot be nested)",
                                ));
                            }
                            texto(e, "value")?
                        }
                        Value::String(s) => Some(s.clone()),
                        _ => None,
                    };
                    let Some(valor) = valor else {
                        return Err(ScimError::invalid_value(
                            "Each element of 'members' must have a 'value'",
                        ));
                    };
                    if !miembros.contains(&valor) {
                        miembros.push(valor);
                    }
                }
            }
            Some(_) => {
                return Err(ScimError::invalid_value(
                    "The attribute 'members' must be a list",
                ));
            }
        }
        Ok(EntradaGrupo {
            descripcion,
            external_id: texto(doc, "externalid")?,
            miembros,
        })
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Escritura

fn permisos_json(lista: impl Iterator<Item = Permission>) -> Value {
    Value::Object(
        lista
            .map(|p| (p.as_str().to_string(), Value::Bool(true)))
            .collect(),
    )
}

/// Campo `permissions` de la cuenta con `authenticate` desactivado o no (especificación §5.3).
pub fn permisos_con_activo(actual: &Permissions, activo: bool) -> Value {
    let (tipo, mut habilitados, mut deshabilitados) = match actual {
        Permissions::Inherit => ("Merge", Vec::new(), Vec::new()),
        Permissions::Merge(l) => (
            "Merge",
            l.enabled_permissions.iter().copied().collect::<Vec<_>>(),
            l.disabled_permissions.iter().copied().collect::<Vec<_>>(),
        ),
        Permissions::Replace(l) => (
            "Replace",
            l.enabled_permissions.iter().copied().collect::<Vec<_>>(),
            l.disabled_permissions.iter().copied().collect::<Vec<_>>(),
        ),
    };
    deshabilitados.retain(|p| *p != Permission::Authenticate);
    if !activo {
        deshabilitados.push(Permission::Authenticate);
    }
    habilitados.dedup();
    if tipo == "Merge" && habilitados.is_empty() && deshabilitados.is_empty() {
        return json!({"@type": "Inherit"});
    }
    json!({
        "@type": tipo,
        "enabledPermissions": permisos_json(habilitados.into_iter()),
        "disabledPermissions": permisos_json(deshabilitados.into_iter()),
    })
}

/// ¿Hay otra cuenta visible del mismo tipo con ese `externalId`? (único dentro del ámbito, decisión bezenti)
async fn external_id_libre(
    ctx: &Contexto<'_>,
    tipo: Tipo,
    external_id: &str,
    propio: Option<u32>,
) -> ScimResult<()> {
    for id in ctx.ids_external_id(external_id).await? {
        if Some(id) != propio && ctx.cuenta(id, Some(tipo)).await?.is_some() {
            return Err(ScimError::uniqueness(format!(
                "The externalId '{external_id}' is already in use by another {}",
                tipo.nombre()
            )));
        }
    }
    Ok(())
}

/// Crea (`existente` = `None`) o reemplaza un usuario. Devuelve su id.
pub async fn escribe_usuario(
    ctx: &Contexto<'_>,
    existente: Option<&CuentaLeida>,
    entrada: EntradaUsuario,
) -> ScimResult<u32> {
    let (local, dominio) = ctx.resuelve_direccion(&entrada.user_name).await?;
    let inquilino = ctx.dominios.get(&dominio).and_then(|d| d.inquilino);

    if let Some(e) = existente
        && e.id == ctx.token.account_id()
        && !entrada.activo
    {
        return Err(ScimError::forbidden(
            "A SCIM client cannot deprovision or deactivate its own service principal",
        ));
    }
    if let Some(external_id) = &entrada.external_id {
        external_id_libre(ctx, Tipo::Usuario, external_id, existente.map(|e| e.id)).await?;
    }
    if let Some(e) = existente
        && e.inquilino() != inquilino
    {
        return Err(ScimError::invalid_value(
            "The new userName must be in a domain of the same tenant as the account",
        ));
    }

    // Alias: dominios abiertos a SCIM y del mismo inquilino. Se conservan `enable` y `description` de los que ya
    // existían.
    let actuales = match existente.map(|e| &e.cuenta) {
        Some(Account::User(u)) => u.aliases.values().cloned().collect::<Vec<_>>(),
        _ => Vec::new(),
    };
    let mut alias = Map::new();
    for (n, direccion) in entrada.alias.iter().enumerate() {
        let (alias_local, alias_dominio) = ctx.resuelve_direccion(direccion).await?;
        if ctx.dominios.get(&alias_dominio).and_then(|d| d.inquilino) != inquilino {
            return Err(ScimError::invalid_value(format!(
                "The alias '{direccion}' is in a domain of another tenant"
            )));
        }
        let previo = actuales
            .iter()
            .find(|a| a.name == alias_local && a.domain_id.document_id() == alias_dominio);
        let mut datos = json!({
            "name": alias_local,
            "domainId": id_texto(alias_dominio),
            "enable": previo.is_none_or(|p| p.enable),
        });
        if let Some(descripcion) = previo.and_then(|p| p.description.clone()) {
            datos["description"] = Value::from(descripcion);
        }
        alias.insert(n.to_string(), datos);
    }

    let mut datos = Map::new();
    datos.insert("name".into(), Value::from(local));
    datos.insert("domainId".into(), Value::from(id_texto(dominio)));
    datos.insert(
        "description".into(),
        entrada.descripcion.map(Value::from).unwrap_or(Value::Null),
    );
    datos.insert(
        "externalId".into(),
        entrada.external_id.map(Value::from).unwrap_or(Value::Null),
    );
    datos.insert("locale".into(), Value::from(entrada.locale.as_str()));
    datos.insert(
        "timeZone".into(),
        entrada
            .zona
            .map(|z| Value::from(z.as_str()))
            .unwrap_or(Value::Null),
    );
    datos.insert("aliases".into(), Value::Object(alias));

    match existente {
        None => {
            datos.insert("@type".into(), Value::from("User"));
            datos.insert("roles".into(), json!({"@type": "User"}));
            datos.insert(
                "permissions".into(),
                permisos_con_activo(&Permissions::Inherit, entrada.activo),
            );
            if let Some(t) = inquilino {
                datos.insert("memberTenantId".into(), Value::from(id_texto(t)));
            }
            let id = ctx.crea(datos).await?;
            trc::event!(
                Scim(ScimEvent::ResourceCreated),
                Type = "User",
                Id = id_texto(id),
                AccountName = entrada.user_name,
                AccountId = ctx.token.account_id(),
                SpanId = ctx.session.session_id,
            );
            Ok(id)
        }
        Some(e) => {
            let Account::User(usuario) = &e.cuenta else {
                return Err(ScimError::internal("Not a user account"));
            };
            datos.insert(
                "permissions".into(),
                permisos_con_activo(&usuario.permissions, entrada.activo),
            );
            ctx.actualiza(vec![(e.id, datos)]).await?;
            Ok(e.id)
        }
    }
}

/// Parte local derivada del nombre de un grupo: minúsculas y cada racha de caracteres que no son letra ni dígito
/// cambiada por `-`.
pub fn local_de_grupo(nombre: &str) -> String {
    let mut out = String::new();
    let mut guion = false;
    for c in nombre.chars().flat_map(|c| c.to_lowercase()) {
        if c.is_alphanumeric() {
            out.push(c);
            guion = false;
        } else if !guion && !out.is_empty() {
            out.push('-');
            guion = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    let mut out: String = out.chars().take(MAX_LOCAL).collect();
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() {
        "group".to_string()
    } else {
        out
    }
}

/// ¿Hay otro grupo visible del mismo inquilino con ese nombre? (sin distinguir mayúsculas)
async fn nombre_grupo_libre(
    ctx: &Contexto<'_>,
    nombre: &str,
    inquilino: Option<u32>,
    propio: Option<u32>,
) -> ScimResult<()> {
    for id in ctx.ids_visibles(Tipo::Grupo).await? {
        if Some(id) == propio {
            continue;
        }
        if let Some(grupo) = ctx.cuenta(id, Some(Tipo::Grupo)).await?
            && grupo.inquilino() == inquilino
            && nombre_visible(&grupo).eq_ignore_ascii_case(nombre)
        {
            return Err(ScimError::uniqueness(format!(
                "A group named '{nombre}' already exists"
            )));
        }
    }
    Ok(())
}

/// Resuelve los miembros pedidos: usuarios visibles.
async fn resuelve_miembros(ctx: &Contexto<'_>, miembros: &[String]) -> ScimResult<BTreeSet<u32>> {
    let mut out = BTreeSet::new();
    for valor in miembros {
        let Some(id) = id_de_texto(valor) else {
            return Err(ScimError::invalid_value(format!(
                "Member '{valor}' not found"
            )));
        };
        match ctx.cuenta(id, None).await? {
            Some(c) if c.tipo() == Tipo::Usuario => {
                out.insert(id);
            }
            Some(_) => {
                return Err(ScimError::invalid_value(
                    "Only users can be members of a group (groups cannot be nested)",
                ));
            }
            None => {
                return Err(ScimError::invalid_value(format!(
                    "Member '{valor}' not found"
                )));
            }
        }
    }
    Ok(out)
}

/// Pone o quita el grupo en `memberGroupIds` de cada usuario.
async fn cambia_miembros(
    ctx: &Contexto<'_>,
    grupo: u32,
    altas: &BTreeSet<u32>,
    bajas: &BTreeSet<u32>,
) -> ScimResult<()> {
    let mut cambios = Vec::new();
    for (id, alta) in altas
        .iter()
        .map(|id| (*id, true))
        .chain(bajas.iter().map(|id| (*id, false)))
    {
        let Some(usuario) = ctx.cuenta(id, Some(Tipo::Usuario)).await? else {
            continue;
        };
        let Account::User(u) = &usuario.cuenta else {
            continue;
        };
        let mut grupos: BTreeSet<u32> =
            u.member_group_ids.iter().map(|g| g.document_id()).collect();
        let cambia = if alta {
            grupos.insert(grupo)
        } else {
            grupos.remove(&grupo)
        };
        if cambia {
            let mut datos = Map::new();
            datos.insert(
                "memberGroupIds".into(),
                Value::Object(
                    grupos
                        .into_iter()
                        .map(|g| (id_texto(g), Value::Bool(true)))
                        .collect(),
                ),
            );
            cambios.push((id, datos));
        }
    }
    ctx.actualiza(cambios).await
}

/// Crea (`existente` = `None`) o reemplaza un grupo. Devuelve su id.
pub async fn escribe_grupo(
    ctx: &Contexto<'_>,
    existente: Option<&CuentaLeida>,
    entrada: EntradaGrupo,
) -> ScimResult<u32> {
    let miembros = resuelve_miembros(ctx, &entrada.miembros).await?;
    if let Some(external_id) = &entrada.external_id {
        external_id_libre(ctx, Tipo::Grupo, external_id, existente.map(|e| e.id)).await?;
    }

    match existente {
        None => {
            if !miembros.is_empty() {
                ctx.exige(Permission::SysAccountUpdate)?;
            }
            // El grupo va al dominio de la cuenta de aprovisionamiento (la petición no trae dirección).
            let dominio = match ctx.cuenta_propia().await? {
                Some(Account::User(u)) => u.domain_id.document_id(),
                Some(Account::Group(g)) => g.domain_id.document_id(),
                None => {
                    return Err(ScimError::invalid_value(
                        "The provisioning account was not found",
                    ));
                }
            };
            let Some(info) = ctx.dominios.get(&dominio) else {
                return Err(ScimError::invalid_value(
                    "The provisioning account's domain is not open to SCIM, so groups cannot be created",
                ));
            };
            let inquilino = info.inquilino;
            nombre_grupo_libre(ctx, &entrada.descripcion, inquilino, None).await?;

            let base = local_de_grupo(&entrada.descripcion);
            let mut local = None;
            for n in 1..=100u32 {
                let candidato = if n == 1 {
                    base.clone()
                } else {
                    let sufijo = format!("-{n}");
                    let corte: String = base.chars().take(MAX_LOCAL - sufijo.len()).collect();
                    format!("{}{sufijo}", corte.trim_end_matches('-'))
                };
                if ctx.direccion_libre(&candidato, dominio).await? {
                    local = Some(candidato);
                    break;
                }
            }
            let Some(local) = local else {
                return Err(ScimError::conflict(
                    "No free address could be derived from the group's displayName",
                ));
            };

            let mut datos = Map::new();
            datos.insert("@type".into(), Value::from("Group"));
            datos.insert("name".into(), Value::from(local.clone()));
            datos.insert("domainId".into(), Value::from(id_texto(dominio)));
            datos.insert(
                "description".into(),
                Value::from(entrada.descripcion.clone()),
            );
            if let Some(external_id) = &entrada.external_id {
                datos.insert("externalId".into(), Value::from(external_id.as_str()));
            }
            if let Some(t) = inquilino {
                datos.insert("memberTenantId".into(), Value::from(id_texto(t)));
            }
            let id = ctx.crea(datos).await?;
            if let Err(err) = cambia_miembros(ctx, id, &miembros, &BTreeSet::new()).await {
                // Deshacer: borrar el grupo recién creado.
                if ctx.destruye(id).await.is_err() {
                    trc::event!(
                        Scim(ScimEvent::RollbackFailed),
                        Id = id_texto(id),
                        Details = "Could not remove a group whose members could not be set",
                        SpanId = ctx.session.session_id,
                    );
                }
                return Err(err);
            }
            trc::event!(
                Scim(ScimEvent::ResourceCreated),
                Type = "Group",
                Id = id_texto(id),
                AccountName = format!(
                    "{local}@{}",
                    ctx.nombre_dominio(dominio).unwrap_or_default()
                ),
                AccountId = ctx.token.account_id(),
                SpanId = ctx.session.session_id,
            );
            Ok(id)
        }
        Some(e) => {
            nombre_grupo_libre(ctx, &entrada.descripcion, e.inquilino(), Some(e.id)).await?;
            let actuales: BTreeSet<u32> = miembros_visibles(ctx, e.id)
                .await?
                .into_iter()
                .map(|m| m.id)
                .collect();
            let altas: BTreeSet<u32> = miembros.difference(&actuales).copied().collect();
            let bajas: BTreeSet<u32> = actuales.difference(&miembros).copied().collect();
            if !altas.is_empty() || !bajas.is_empty() {
                ctx.exige(Permission::SysAccountUpdate)?;
            }
            let mut datos = Map::new();
            datos.insert("description".into(), Value::from(entrada.descripcion));
            datos.insert(
                "externalId".into(),
                entrada.external_id.map(Value::from).unwrap_or(Value::Null),
            );
            ctx.actualiza(vec![(e.id, datos)]).await?;
            cambia_miembros(ctx, e.id, &altas, &bajas).await?;
            Ok(e.id)
        }
    }
}

/// Borra un usuario o un grupo (a un grupo se le quitan antes los miembros, que lo referencian).
pub async fn borra(ctx: &Contexto<'_>, leida: &CuentaLeida) -> ScimResult<()> {
    if leida.id == ctx.token.account_id() {
        return Err(ScimError::forbidden(
            "A SCIM client cannot deprovision or deactivate its own service principal",
        ));
    }
    if leida.tipo() == Tipo::Grupo {
        let todos: BTreeSet<u32> = ctx.ids_miembros(leida.id).await?.into_iter().collect();
        if !todos.is_empty() {
            cambia_miembros(ctx, leida.id, &BTreeSet::new(), &todos).await?;
        }
    }
    ctx.destruye(leida.id).await?;
    trc::event!(
        Scim(ScimEvent::ResourceDeleted),
        Type = leida.tipo().nombre(),
        Id = id_texto(leida.id),
        AccountId = ctx.token.account_id(),
        SpanId = ctx.session.session_id,
    );
    Ok(())
}

#[cfg(test)]
mod pruebas {
    use super::*;

    #[test]
    fn direccion_de_grupo() {
        assert_eq!(local_de_grupo("Sales EMEA"), "sales-emea");
        assert_eq!(
            local_de_grupo("  Ventas & Marketing!! "),
            "ventas-marketing"
        );
        assert_eq!(local_de_grupo("Diseño"), "diseño");
        assert_eq!(local_de_grupo("***"), "group");
        assert_eq!(local_de_grupo(&"a".repeat(100)).len(), 64);
    }

    #[test]
    fn locales_y_zonas() {
        assert_eq!(normaliza_locale("EN-us"), Some(Locale::EnUS));
        assert_eq!(normaliza_locale("en_US"), Some(Locale::EnUS));
        assert!(normaliza_locale("es-PE").is_some());
        assert!(normaliza_locale("xx-YY").is_none());
        assert!(normaliza_zona("Europe/Lisbon").is_some());
        assert!(normaliza_zona("america/lima").is_some());
        assert!(normaliza_zona("Marte/Olimpo").is_none());
    }

    #[test]
    fn activo_en_permisos() {
        let inactivo = permisos_con_activo(&Permissions::Inherit, false);
        assert_eq!(inactivo["@type"], "Merge");
        assert_eq!(inactivo["disabledPermissions"]["authenticate"], true);
        let reactivado: Permissions = serde_json::from_value(inactivo).unwrap();
        assert_eq!(
            permisos_con_activo(&reactivado, true),
            json!({"@type": "Inherit"})
        );
        assert_eq!(
            permisos_con_activo(&Permissions::Inherit, true),
            json!({"@type": "Inherit"})
        );
    }

    #[test]
    fn atributos_conocidos_y_desconocidos() {
        let ok = json!({
            "schemas": [Tipo::Usuario.urn(), URN_ENTERPRISE],
            "userName": "a@b.test", "title": "CEO", "password": "x",
            "phoneNumbers": [{"value": "1"}],
            URN_ENTERPRISE: {"employeeNumber": "7", "manager": {"value": "m"}},
        });
        let m = valida_atributos(Tipo::Usuario, &ok).unwrap();
        assert!(
            m.contains_key("username") && !m.contains_key("title") && !m.contains_key("password")
        );
        for malo in [
            json!({"userName": "a@b.test"}),
            json!({"schemas": [Tipo::Usuario.urn()], "dispalyName": "x"}),
            json!({"schemas": [Tipo::Usuario.urn(), "urn:x:y"], "userName": "a@b.test"}),
            json!({"schemas": [Tipo::Usuario.urn()], "userName": "a", "USERNAME": "b"}),
            json!({"schemas": [Tipo::Usuario.urn()], URN_ENTERPRISE: {"shoeSize": 9}}),
        ] {
            let err = valida_atributos(Tipo::Usuario, &malo).unwrap_err();
            assert_eq!(err.scim_type, Some("invalidSyntax"), "{malo}");
        }
    }

    #[test]
    fn nombre_compuesto() {
        let doc = valida_atributos(
            Tipo::Usuario,
            &json!({"schemas": [Tipo::Usuario.urn()], "userName": "Ana@B.test",
                    "name": {"givenName": "Ana", "familyName": "Pérez"}, "active": "False"}),
        )
        .unwrap();
        let e = EntradaUsuario::de(&doc).unwrap();
        assert_eq!(e.descripcion.as_deref(), Some("Ana Pérez"));
        assert_eq!(e.user_name, "ana@b.test");
        assert!(!e.activo);
    }
}
