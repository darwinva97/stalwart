/*
 * SPDX-FileCopyrightText: 2026 bezenti
 *
 * SPDX-License-Identifier: AGPL-3.0-only
 */

//! Contexto de una petición SCIM: quién llama, qué ve (dominios abiertos a SCIM dentro de su ámbito de inquilino)
//! y el acceso al registro.
//!
//! - Lecturas: directamente del registro, filtradas por el ámbito (especificación §4).
//! - Escrituras: siempre por `registry_set` (el mismo `x:Account/set` de la API JMAP), para que se apliquen todas
//!   las reglas: aislamiento y cuotas de inquilino (fase 2), regla de modificación de cuentas, direcciones únicas,
//!   invalidación de cachés y tarea de borrado de datos.

use crate::{
    error::{ScimError, ScimResult},
    esquema::Tipo,
};
use common::{Server, auth::AccessToken, bezenti::inquilino::TenantScope};
use http_proto::HttpSessionData;
use jmap::registry::set::RegistrySet;
use jmap_proto::{method::set::SetRequest, object::registry::Registry};
use registry::{
    schema::{
        enums::{AccountType, Permission},
        prelude::{ObjectType, Property},
        structs::{Account, Domain},
    },
    types::EnumImpl,
};
use serde_json::{Map, Value, json};
use std::{collections::HashMap, sync::Mutex};
use store::{registry::RegistryQuery, roaring::RoaringBitmap};
use trc::AddContext;
use types::id::Id;

/// Un dominio abierto a SCIM que quien llama puede ver.
#[derive(Debug, Clone)]
pub struct DominioScim {
    pub nombre: String,
    pub inquilino: Option<u32>,
}

pub struct Contexto<'x> {
    pub server: &'x Server,
    pub token: &'x AccessToken,
    pub session: &'x HttpSessionData,
    /// URL absoluta de la base SCIM (`https://host/scim/v2`).
    pub base: String,
    /// `None`: operador (sin inquilino).
    pub ambito: Option<TenantScope>,
    /// Dominios abiertos a SCIM dentro del ámbito.
    pub dominios: HashMap<u32, DominioScim>,
    /// Usuarios visibles (se calcula una vez por petición y se olvida tras cada escritura).
    usuarios: Mutex<Option<RoaringBitmap>>,
}

/// Una cuenta leída del registro.
#[derive(Debug, Clone)]
pub struct CuentaLeida {
    pub id: u32,
    pub cuenta: Account,
}

impl CuentaLeida {
    pub fn tipo(&self) -> Tipo {
        match self.cuenta {
            Account::User(_) => Tipo::Usuario,
            Account::Group(_) => Tipo::Grupo,
        }
    }

    pub fn dominio(&self) -> u32 {
        match &self.cuenta {
            Account::User(u) => u.domain_id.document_id(),
            Account::Group(g) => g.domain_id.document_id(),
        }
    }

    pub fn inquilino(&self) -> Option<u32> {
        match &self.cuenta {
            Account::User(u) => u.member_tenant_id.map(|t| t.document_id()),
            Account::Group(g) => g.member_tenant_id.map(|t| t.document_id()),
        }
    }
}

pub fn id_texto(id: u32) -> String {
    Id::from(id).to_string()
}

pub fn id_de_texto(texto: &str) -> Option<u32> {
    texto
        .parse::<Id>()
        .ok()
        .and_then(|id| u32::try_from(id.id()).ok())
}

impl<'x> Contexto<'x> {
    pub async fn nuevo(
        server: &'x Server,
        token: &'x AccessToken,
        session: &'x HttpSessionData,
        base: String,
    ) -> ScimResult<Contexto<'x>> {
        let ambito = server.access_token_scope(token).await?;
        let mut dominios = HashMap::new();
        let ids = server
            .registry()
            .query::<RoaringBitmap>(RegistryQuery::new(ObjectType::Domain))
            .await
            .caused_by(trc::location!())?;
        for id in ids {
            let Some(dominio) = server
                .registry()
                .object::<Domain>(Id::from(id))
                .await
                .caused_by(trc::location!())?
            else {
                continue;
            };
            let inquilino = dominio.member_tenant_id.map(|t| t.document_id());
            let en_ambito = match (&ambito, inquilino) {
                (None, _) => true,
                (Some(ambito), Some(t)) => ambito.contains(t),
                (Some(_), None) => false,
            };
            if dominio.allow_scim_provisioning && en_ambito {
                dominios.insert(
                    id,
                    DominioScim {
                        nombre: dominio.name.to_ascii_lowercase(),
                        inquilino,
                    },
                );
            }
        }
        Ok(Contexto {
            server,
            token,
            session,
            base,
            ambito,
            dominios,
            usuarios: Mutex::new(None),
        })
    }

    /// Usuarios visibles (en caché durante la petición).
    pub async fn usuarios_visibles(&self) -> ScimResult<RoaringBitmap> {
        if let Some(ids) = self.usuarios.lock().ok().and_then(|g| g.clone()) {
            return Ok(ids);
        }
        let ids = self.ids_visibles(Tipo::Usuario).await?;
        if let Ok(mut g) = self.usuarios.lock() {
            *g = Some(ids.clone());
        }
        Ok(ids)
    }

    fn olvida_usuarios(&self) {
        if let Ok(mut g) = self.usuarios.lock() {
            *g = None;
        }
    }

    /// Usuarios visibles miembros de un grupo, ordenados por id (sin leer cada cuenta).
    pub async fn ids_miembros_visibles(&self, grupo: u32) -> ScimResult<Vec<u32>> {
        let mut ids = self.ids_miembros(grupo).await?;
        ids &= self.usuarios_visibles().await?;
        Ok(ids.into_iter().collect())
    }

    pub fn exige(&self, permiso: Permission) -> ScimResult<()> {
        if self.token.has_permission(permiso) {
            Ok(())
        } else {
            Err(ScimError::forbidden(format!(
                "The provisioning account lacks the '{}' permission",
                permiso.as_str()
            )))
        }
    }

    fn inquilino_en_ambito(&self, inquilino: Option<u32>) -> bool {
        match (&self.ambito, inquilino) {
            (None, _) => true,
            (Some(ambito), Some(t)) => ambito.contains(t),
            (Some(_), None) => false,
        }
    }

    /// Cuenta visible (dominio abierto y dentro del ámbito), del tipo pedido si se indica.
    pub async fn cuenta(&self, id: u32, tipo: Option<Tipo>) -> ScimResult<Option<CuentaLeida>> {
        let Some(cuenta) = self
            .server
            .registry()
            .object::<Account>(Id::from(id))
            .await
            .caused_by(trc::location!())?
        else {
            return Ok(None);
        };
        let leida = CuentaLeida { id, cuenta };
        if self.dominios.contains_key(&leida.dominio())
            && self.inquilino_en_ambito(leida.inquilino())
            && tipo.is_none_or(|t| t == leida.tipo())
        {
            Ok(Some(leida))
        } else {
            Ok(None)
        }
    }

    /// La cuenta con la que se autentica el cliente SCIM (sin filtrar por ámbito).
    pub async fn cuenta_propia(&self) -> ScimResult<Option<Account>> {
        Ok(self
            .server
            .registry()
            .object::<Account>(Id::from(self.token.account_id()))
            .await
            .caused_by(trc::location!())?)
    }

    /// Ids de todas las cuentas visibles de un tipo.
    pub async fn ids_visibles(&self, tipo: Tipo) -> ScimResult<RoaringBitmap> {
        let tipo_cuenta = match tipo {
            Tipo::Usuario => AccountType::User,
            Tipo::Grupo => AccountType::Group,
        };
        let mut ids = RoaringBitmap::new();
        for dominio in self.dominios.keys() {
            ids |= self
                .server
                .registry()
                .query::<RoaringBitmap>(
                    RegistryQuery::new(ObjectType::Account)
                        .equal(Property::DomainId, *dominio as u64)
                        .equal(Property::Type, tipo_cuenta.to_id()),
                )
                .await
                .caused_by(trc::location!())?;
        }
        if let Some(ambito) = &self.ambito {
            ids &= self
                .server
                .tenant_scope_object_ids(ObjectType::Account, ambito)
                .await?;
        }
        Ok(ids)
    }

    /// Cuentas con un `externalId` dado (índice exacto), sin filtrar por ámbito.
    pub async fn ids_external_id(&self, external_id: &str) -> ScimResult<RoaringBitmap> {
        Ok(self
            .server
            .registry()
            .query::<RoaringBitmap>(
                RegistryQuery::new(ObjectType::Account)
                    .equal(Property::ExternalId, external_id.to_string()),
            )
            .await
            .caused_by(trc::location!())?)
    }

    /// Usuarios miembros de un grupo (índice `memberGroupIds`), sin filtrar por ámbito.
    pub async fn ids_miembros(&self, grupo: u32) -> ScimResult<RoaringBitmap> {
        Ok(self
            .server
            .registry()
            .query::<RoaringBitmap>(
                RegistryQuery::new(ObjectType::Account)
                    .equal(Property::MemberGroupIds, grupo as u64),
            )
            .await
            .caused_by(trc::location!())?)
    }

    /// Cuenta dueña de una dirección (principal o alias), sin filtrar por ámbito.
    pub async fn id_de_direccion(&self, direccion: &str) -> ScimResult<Option<u32>> {
        Ok(self
            .server
            .account_id_from_email(&direccion.to_lowercase(), false)
            .await?)
    }

    /// ¿Está libre la dirección `local@dominio` (ni cuenta, ni alias, ni lista)?
    pub async fn direccion_libre(&self, local: &str, dominio: u32) -> ScimResult<bool> {
        Ok(self
            .server
            .rcpt_id_from_parts(local, dominio)
            .await?
            .is_none())
    }

    /// Separa una dirección y comprueba que su dominio existe, es visible y está abierto a SCIM.
    pub async fn resuelve_direccion(&self, direccion: &str) -> ScimResult<(String, u32)> {
        let direccion = direccion.trim().to_lowercase();
        let Some((local, dominio)) = direccion.rsplit_once('@') else {
            return Err(ScimError::invalid_value(format!(
                "'{direccion}' is not an email address: userName and emails must be full addresses"
            )));
        };
        if local.is_empty() || dominio.is_empty() {
            return Err(ScimError::invalid_value(format!(
                "'{direccion}' is not a valid email address"
            )));
        }
        // Mismo mensaje si no existe o si no es visible: no se revela qué dominios tienen otros inquilinos.
        match self.server.domain(dominio).await? {
            Some(cache) if self.dominios.contains_key(&cache.id) => {
                Ok((local.to_string(), cache.id))
            }
            _ => Err(ScimError::invalid_value(format!(
                "The domain '{dominio}' does not exist or is not open to SCIM provisioning"
            ))),
        }
    }

    pub fn nombre_dominio(&self, id: u32) -> Option<&str> {
        self.dominios.get(&id).map(|d| d.nombre.as_str())
    }

    /// Nombre de un dominio cualquiera (también los no abiertos: alias heredados de antes de SCIM).
    pub async fn nombre_dominio_cualquiera(&self, id: u32) -> ScimResult<String> {
        if let Some(nombre) = self.nombre_dominio(id) {
            return Ok(nombre.to_string());
        }
        Ok(self
            .server
            .domain_by_id(id)
            .await?
            .and_then(|d| d.names.first().map(|n| n.to_string()))
            .unwrap_or_default())
    }

    /// ¿Incluyen los permisos efectivos de la cuenta `authenticate`? (roles + `permissions` + techo del inquilino)
    pub async fn puede_autenticarse(&self, cuenta: &Account) -> ScimResult<bool> {
        Ok(self
            .server
            .account_permissions(cuenta)
            .await?
            .finalize_as_ref()
            .get(Permission::Authenticate as usize))
    }

    /// Ejecuta un `x:Account/set` con los permisos de quien llama. Devuelve la respuesta en JSON.
    pub async fn registry_set(
        &self,
        create: Option<Map<String, Value>>,
        update: Option<Map<String, Value>>,
        destroy: Option<Vec<String>>,
    ) -> ScimResult<Value> {
        let mut peticion = json!({ "accountId": id_texto(self.token.account_id()) });
        if let Some(create) = create {
            peticion["create"] = Value::Object(create);
        }
        if let Some(update) = update {
            peticion["update"] = Value::Object(update);
        }
        if let Some(destroy) = destroy {
            peticion["destroy"] = json!(destroy);
        }
        self.olvida_usuarios();
        let texto = peticion.to_string();
        let request: SetRequest<'_, Registry> = serde_json::from_str(&texto).map_err(|err| {
            ScimError::internal(format!("Failed to build the registry request: {err}"))
        })?;
        let response = self
            .server
            .registry_set(ObjectType::Account, request, self.token, self.session)
            .await?;
        serde_json::to_value(&response).map_err(|err| {
            ScimError::internal(format!("Failed to read the registry response: {err}"))
        })
    }

    /// Crea una cuenta. Devuelve su id o el error SCIM.
    pub async fn crea(&self, datos: Map<String, Value>) -> ScimResult<u32> {
        let mut create = Map::new();
        create.insert("n".to_string(), Value::Object(datos));
        let r = self.registry_set(Some(create), None, None).await?;
        if let Some(id) = r
            .pointer("/created/n/id")
            .and_then(|v| v.as_str())
            .and_then(id_de_texto)
        {
            Ok(id)
        } else if let Some(err) = r.pointer("/notCreated/n") {
            Err(ScimError::from_set_error(err))
        } else {
            Err(ScimError::internal(
                "The registry did not create the account",
            ))
        }
    }

    /// Cambia varias cuentas a la vez. Devuelve el primer error.
    pub async fn actualiza(&self, cambios: Vec<(u32, Map<String, Value>)>) -> ScimResult<()> {
        if cambios.is_empty() {
            return Ok(());
        }
        let ids: Vec<String> = cambios.iter().map(|(id, _)| id_texto(*id)).collect();
        let update: Map<String, Value> = cambios
            .into_iter()
            .map(|(id, c)| (id_texto(id), Value::Object(c)))
            .collect();
        let r = self.registry_set(None, Some(update), None).await?;
        for id in ids {
            if let Some(err) = r.get("notUpdated").and_then(|n| n.get(&id)) {
                return Err(ScimError::from_set_error(err));
            }
        }
        Ok(())
    }

    pub async fn destruye(&self, id: u32) -> ScimResult<()> {
        let texto = id_texto(id);
        let r = self
            .registry_set(None, None, Some(vec![texto.clone()]))
            .await?;
        if let Some(err) = r.get("notDestroyed").and_then(|n| n.get(&texto)) {
            Err(ScimError::from_set_error(err))
        } else {
            Ok(())
        }
    }
}
