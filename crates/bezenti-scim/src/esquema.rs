/*
 * SPDX-FileCopyrightText: 2026 bezenti
 *
 * SPDX-License-Identifier: AGPL-3.0-only
 */

//! Descubrimiento (`/ServiceProviderConfig`, `/ResourceTypes`, `/Schemas`, RFC 7643 §5–7) y catálogo de
//! atributos: los que se guardan (y se publican) y los que se aceptan y se descartan.
//! Especificación: `bezenti/especificaciones/scim.md`, §2 y §5.

use serde_json::{Value, json};

pub const URN_USER: &str = "urn:ietf:params:scim:schemas:core:2.0:User";
pub const URN_GROUP: &str = "urn:ietf:params:scim:schemas:core:2.0:Group";
pub const URN_ENTERPRISE: &str = "urn:ietf:params:scim:schemas:extension:enterprise:2.0:User";
pub const URN_LIST: &str = "urn:ietf:params:scim:api:messages:2.0:ListResponse";
pub const URN_PATCH: &str = "urn:ietf:params:scim:api:messages:2.0:PatchOp";
pub const URN_SEARCH: &str = "urn:ietf:params:scim:api:messages:2.0:SearchRequest";
pub const URN_BULK_REQUEST: &str = "urn:ietf:params:scim:api:messages:2.0:BulkRequest";
pub const URN_BULK_RESPONSE: &str = "urn:ietf:params:scim:api:messages:2.0:BulkResponse";
pub const URN_SPC: &str = "urn:ietf:params:scim:schemas:core:2.0:ServiceProviderConfig";
pub const URN_RESOURCE_TYPE: &str = "urn:ietf:params:scim:schemas:core:2.0:ResourceType";
pub const URN_SCHEMA: &str = "urn:ietf:params:scim:schemas:core:2.0:Schema";

pub const MAX_BULK_OPERATIONS: usize = 1000;
pub const MAX_PAYLOAD: usize = 1024 * 1024;
pub const MAX_RESULTS: usize = 200;
pub const DEFAULT_PAGE: usize = 100;

/// Tipo de recurso.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Tipo {
    Usuario,
    Grupo,
}

impl Tipo {
    pub fn endpoint(&self) -> &'static str {
        match self {
            Tipo::Usuario => "Users",
            Tipo::Grupo => "Groups",
        }
    }

    pub fn nombre(&self) -> &'static str {
        match self {
            Tipo::Usuario => "User",
            Tipo::Grupo => "Group",
        }
    }

    pub fn urn(&self) -> &'static str {
        match self {
            Tipo::Usuario => URN_USER,
            Tipo::Grupo => URN_GROUP,
        }
    }

    pub fn de_endpoint(endpoint: &str) -> Option<Tipo> {
        match endpoint {
            "Users" => Some(Tipo::Usuario),
            "Groups" => Some(Tipo::Grupo),
            _ => None,
        }
    }
}

/// Atributos (de primer nivel, en minúsculas) que se guardan.
pub fn guardado(tipo: Tipo, atributo: &str) -> bool {
    match tipo {
        Tipo::Usuario => matches!(
            atributo,
            "id" | "externalid"
                | "username"
                | "name"
                | "displayname"
                | "active"
                | "emails"
                | "locale"
                | "preferredlanguage"
                | "timezone"
                | "groups"
                | "meta"
                | "schemas"
        ),
        Tipo::Grupo => matches!(
            atributo,
            "id" | "externalid" | "displayname" | "members" | "meta" | "schemas"
        ),
    }
}

/// Atributos de un esquema conocido que se aceptan pero no se guardan (RFC 7643 §4.1, §4.2, §4.3).
pub fn conocido_no_guardado(tipo: Tipo, atributo: &str) -> bool {
    match tipo {
        Tipo::Usuario => matches!(
            atributo,
            "nickname"
                | "profileurl"
                | "title"
                | "usertype"
                | "password"
                | "phonenumbers"
                | "ims"
                | "photos"
                | "addresses"
                | "entitlements"
                | "roles"
                | "x509certificates"
        ),
        // Okta y otros mandan `description` en grupos; no está en RFC 7643 pero la doc lo cita como descartado.
        Tipo::Grupo => matches!(atributo, "description"),
    }
}

/// Atributos de la extensión empresarial (se aceptan y se descartan).
pub fn atributo_empresa(atributo: &str) -> bool {
    matches!(
        atributo,
        "employeenumber" | "costcenter" | "organization" | "division" | "department" | "manager"
    )
}

/// ¿Es una URI de esquema que conocemos para este tipo de recurso?
pub fn uri_conocida(tipo: Tipo, uri: &str) -> bool {
    uri.eq_ignore_ascii_case(tipo.urn())
        || (tipo == Tipo::Usuario && uri.eq_ignore_ascii_case(URN_ENTERPRISE))
}

/// Atributos filtrables y ordenables (`nombre` o `nombre.sub`, en minúsculas). Especificación §6.
pub fn filtrable(tipo: Tipo, clave: &str) -> bool {
    match tipo {
        Tipo::Usuario => matches!(
            clave,
            "id" | "externalid"
                | "username"
                | "displayname"
                | "name.formatted"
                | "name.givenname"
                | "name.familyname"
                | "emails"
                | "emails.value"
                | "emails.type"
                | "emails.primary"
                | "active"
                | "locale"
                | "preferredlanguage"
                | "timezone"
                | "groups"
                | "groups.value"
                | "groups.display"
                | "meta.created"
                | "meta.resourcetype"
        ),
        Tipo::Grupo => matches!(
            clave,
            "id" | "externalid"
                | "displayname"
                | "members"
                | "members.value"
                | "members.display"
                | "meta.created"
                | "meta.resourcetype"
        ),
    }
}

/// `/ServiceProviderConfig` (doc: valores fijos).
pub fn service_provider_config(base: &str) -> Value {
    json!({
        "schemas": [URN_SPC],
        "documentationUri": "https://stalw.art/docs/auth/scim/",
        "patch": {"supported": true},
        "bulk": {
            "supported": true,
            "maxOperations": MAX_BULK_OPERATIONS,
            "maxPayloadSize": MAX_PAYLOAD
        },
        "filter": {"supported": true, "maxResults": MAX_RESULTS},
        "changePassword": {"supported": false},
        "sort": {"supported": true},
        "etag": {"supported": true},
        "pagination": {
            "cursor": true,
            "index": true,
            "defaultPaginationMethod": "index",
            "defaultPageSize": DEFAULT_PAGE,
            "maxPageSize": MAX_RESULTS,
            "cursorTimeout": 3600
        },
        "authenticationSchemes": [{
            "type": "oauthbearertoken",
            "name": "OAuth Bearer Token",
            "description": "Authentication with an API key (or OAuth access token) presented as an HTTP bearer token",
            "specUri": "https://www.rfc-editor.org/info/rfc6750",
            "documentationUri": "https://stalw.art/docs/auth/scim/configuration/",
            "primary": true
        }],
        "meta": {
            "resourceType": "ServiceProviderConfig",
            "location": format!("{base}/ServiceProviderConfig")
        }
    })
}

/// `/ResourceTypes/{User,Group}`.
pub fn resource_type(base: &str, tipo: Tipo) -> Value {
    json!({
        "schemas": [URN_RESOURCE_TYPE],
        "id": tipo.nombre(),
        "name": tipo.nombre(),
        "endpoint": format!("/{}", tipo.endpoint()),
        "description": match tipo {
            Tipo::Usuario => "User account",
            Tipo::Grupo => "Group account",
        },
        "schema": tipo.urn(),
        "meta": {
            "resourceType": "ResourceType",
            "location": format!("{base}/ResourceTypes/{}", tipo.nombre())
        }
    })
}

fn atributo(
    nombre: &str,
    tipo: &str,
    multi: bool,
    descripcion: &str,
    requerido: bool,
    case_exact: bool,
    mutabilidad: &str,
    devuelto: &str,
    unicidad: &str,
) -> Value {
    json!({
        "name": nombre,
        "type": tipo,
        "multiValued": multi,
        "description": descripcion,
        "required": requerido,
        "caseExact": case_exact,
        "mutability": mutabilidad,
        "returned": devuelto,
        "uniqueness": unicidad
    })
}

fn con_sub(mut atributo: Value, sub: Vec<Value>) -> Value {
    atributo["subAttributes"] = Value::Array(sub);
    atributo
}

fn referencia(nombre: &str, tipos: &[&str], mutabilidad: &str) -> Value {
    let mut a = atributo(
        nombre,
        "reference",
        false,
        "The URI of the referenced resource",
        false,
        false,
        mutabilidad,
        "default",
        "none",
    );
    a["referenceTypes"] = json!(tipos);
    a
}

/// `/Schemas/{urn}`: solo los atributos que se guardan (doc).
pub fn schema(base: &str, tipo: Tipo) -> Value {
    let comunes = |a: &mut Vec<Value>| {
        a.push(atributo(
            "externalId",
            "string",
            false,
            "Identifier of the resource in the provisioning client",
            false,
            true,
            "readWrite",
            "default",
            "none",
        ));
    };
    let mut atributos = Vec::new();
    match tipo {
        Tipo::Usuario => {
            let mut user_name = atributo(
                "userName",
                "string",
                false,
                "Email address of the account (local part and domain)",
                true,
                false,
                "readWrite",
                "default",
                "server",
            );
            user_name["required"] = Value::Bool(true);
            atributos.push(user_name);
            comunes(&mut atributos);
            atributos.push(con_sub(
                atributo(
                    "name",
                    "complex",
                    false,
                    "The name of the user (only the formatted name is stored)",
                    false,
                    false,
                    "readWrite",
                    "default",
                    "none",
                ),
                vec![atributo(
                    "formatted",
                    "string",
                    false,
                    "Full name, stored as the account description",
                    false,
                    false,
                    "readWrite",
                    "default",
                    "none",
                )],
            ));
            atributos.push(atributo(
                "displayName",
                "string",
                false,
                "Name of the user, stored as the account description",
                false,
                false,
                "readWrite",
                "default",
                "none",
            ));
            atributos.push(atributo(
                "preferredLanguage",
                "string",
                false,
                "Preferred language (same stored value as locale)",
                false,
                false,
                "readWrite",
                "default",
                "none",
            ));
            atributos.push(atributo(
                "locale",
                "string",
                false,
                "Locale of the account, for example en-US",
                false,
                false,
                "readWrite",
                "default",
                "none",
            ));
            atributos.push(atributo(
                "timezone",
                "string",
                false,
                "Time zone of the account (IANA identifier)",
                false,
                false,
                "readWrite",
                "default",
                "none",
            ));
            atributos.push(atributo(
                "active",
                "boolean",
                false,
                "Whether the account may sign in (the authenticate permission)",
                false,
                false,
                "readWrite",
                "default",
                "none",
            ));
            let mut tipo_email = atributo(
                "type",
                "string",
                false,
                "Type of the address",
                false,
                false,
                "readWrite",
                "default",
                "none",
            );
            tipo_email["canonicalValues"] = json!(["work"]);
            atributos.push(con_sub(
                atributo(
                    "emails",
                    "complex",
                    true,
                    "Primary address (from userName) and aliases of the account",
                    false,
                    false,
                    "readWrite",
                    "default",
                    "none",
                ),
                vec![
                    atributo(
                        "value",
                        "string",
                        false,
                        "Email address",
                        false,
                        false,
                        "readWrite",
                        "default",
                        "none",
                    ),
                    tipo_email,
                    atributo(
                        "primary",
                        "boolean",
                        false,
                        "Whether this is the primary address",
                        false,
                        false,
                        "readWrite",
                        "default",
                        "none",
                    ),
                ],
            ));
            atributos.push(con_sub(
                atributo(
                    "groups",
                    "complex",
                    true,
                    "Groups the user belongs to (managed through the Group resource)",
                    false,
                    false,
                    "readOnly",
                    "default",
                    "none",
                ),
                vec![
                    atributo(
                        "value",
                        "string",
                        false,
                        "Identifier of the group",
                        false,
                        true,
                        "readOnly",
                        "default",
                        "none",
                    ),
                    referencia("$ref", &["Group"], "readOnly"),
                    atributo(
                        "display",
                        "string",
                        false,
                        "Display name of the group",
                        false,
                        false,
                        "readOnly",
                        "default",
                        "none",
                    ),
                    {
                        let mut t = atributo(
                            "type",
                            "string",
                            false,
                            "Membership type (groups are not nested, so always direct)",
                            false,
                            false,
                            "readOnly",
                            "default",
                            "none",
                        );
                        t["canonicalValues"] = json!(["direct", "indirect"]);
                        t
                    },
                ],
            ));
        }
        Tipo::Grupo => {
            atributos.push(atributo(
                "displayName",
                "string",
                false,
                "Name of the group, unique among the groups of the tenant",
                true,
                false,
                "readWrite",
                "default",
                "server",
            ));
            comunes(&mut atributos);
            let mut tipo_miembro = atributo(
                "type",
                "string",
                false,
                "Type of the member",
                false,
                false,
                "immutable",
                "default",
                "none",
            );
            tipo_miembro["canonicalValues"] = json!(["User"]);
            atributos.push(con_sub(
                atributo(
                    "members",
                    "complex",
                    true,
                    "Members of the group (users only)",
                    false,
                    false,
                    "readWrite",
                    "default",
                    "none",
                ),
                vec![
                    atributo(
                        "value",
                        "string",
                        false,
                        "Identifier of the member",
                        false,
                        true,
                        "immutable",
                        "default",
                        "none",
                    ),
                    referencia("$ref", &["User"], "immutable"),
                    atributo(
                        "display",
                        "string",
                        false,
                        "Display name of the member",
                        false,
                        false,
                        "readOnly",
                        "default",
                        "none",
                    ),
                    tipo_miembro,
                ],
            ));
        }
    }
    json!({
        "schemas": [URN_SCHEMA],
        "id": tipo.urn(),
        "name": tipo.nombre(),
        "description": match tipo {
            Tipo::Usuario => "User account",
            Tipo::Grupo => "Group account",
        },
        "attributes": atributos,
        "meta": {
            "resourceType": "Schema",
            "location": format!("{base}/Schemas/{}", tipo.urn())
        }
    })
}

/// `ListResponse` con todos los elementos (descubrimiento).
pub fn lista_completa(recursos: Vec<Value>) -> Value {
    json!({
        "schemas": [URN_LIST],
        "totalResults": recursos.len(),
        "startIndex": 1,
        "itemsPerPage": recursos.len(),
        "Resources": recursos
    })
}
