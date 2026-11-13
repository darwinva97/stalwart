/*
 * SPDX-FileCopyrightText: 2026 bezenti
 *
 * SPDX-License-Identifier: AGPL-3.0-only
 */

//! Aislamiento por inquilino en la API del registro (`x:*/get`, `x:*/query`, `x:*/set`). Edición bezenti, fase 2.
//!
//! Especificación: `bezenti/especificaciones/multi-inquilino.md`, §4 y §8. La regla de oro: para un usuario de
//! un inquilino, lo que está fuera de su ámbito (su inquilino y sus descendientes) **no existe**: no aparece en
//! las consultas, `get` responde `notFound`, `set` responde `notFound` y una clave ajena que apunte fuera es tan
//! inválida como una que no existe.

use common::{
    Server,
    auth::{AccessToken, Permissions},
    bezenti::inquilino::{
        TenantQuotaExceeded, TenantScope, is_shared_service_object, object_quota,
    },
};
use jmap_proto::error::set::{SetError, SetErrorType};
use jmap_tools::Key;
use registry::{
    jmap::{IntoValue, JmapValue},
    schema::{
        enums::Permission,
        prelude::{OBJ_FILTER_TENANT, Object, ObjectInner, ObjectType, Property},
        structs::{self, Account},
    },
    types::EnumImpl,
    types::{
        id::ObjectId,
        index::{IndexBuilder, IndexKey},
    },
};
use store::{registry::RegistryQuery, roaring::RoaringBitmap};
use trc::AddContext;
use types::id::Id;
use utils::outbound::OutboundPolicy;

/// Qué puede ver y tocar quien hace la petición.
#[derive(Debug, Clone, Default)]
pub(crate) struct TenantAccess {
    /// `None`: operador del servidor (sin inquilino), ve todo.
    pub scope: Option<TenantScope>,
    /// Permisos efectivos de quien hace la petición: un usuario de inquilino no puede repartir permisos que
    /// no tiene (ni en cuentas, ni en roles, ni en inquilinos hijos).
    permissions: Permissions,
}

/// Tipos de objeto que un usuario de inquilino puede usar en la API del registro (especificación §4.2).
pub(crate) fn is_tenant_accessible(object_type: ObjectType) -> bool {
    matches!(
        object_type,
        // Objetos de inquilino que se guardan en el registro.
        ObjectType::Account
            | ObjectType::Domain
            | ObjectType::MailingList
            | ObjectType::Role
            | ObjectType::OAuthClient
            | ObjectType::Directory
            | ObjectType::DkimSignature
            | ObjectType::AcmeProvider
            | ObjectType::DnsServer
            // Inquilinos descendientes (revendedores).
            | ObjectType::Tenant
            // Objetos de la propia cuenta.
            | ObjectType::AccountSettings
            | ObjectType::ApiKey
            | ObjectType::AccountPassword
            | ObjectType::AppPassword
            | ObjectType::Passkey
            | ObjectType::PublicKey
            | ObjectType::OAuthGrant
            | ObjectType::SpamTrainingSample
            | ObjectType::MaskedEmail
            | ObjectType::ArchivedItem
    )
}

impl TenantAccess {
    /// Ámbito de quien hace la petición. Falla con `forbidden` si es de un inquilino y el tipo de objeto no es
    /// de los que puede usar (la configuración del servidor, la cola, las tareas… son solo del operador).
    pub async fn new(
        server: &Server,
        access_token: &AccessToken,
        object_type: ObjectType,
    ) -> trc::Result<Self> {
        let scope = server.access_token_scope(access_token).await?;
        if scope.is_some() && !is_tenant_accessible(object_type) {
            return Err(trc::JmapEvent::Forbidden
                .into_err()
                .details("This object type can only be managed by the server administrator"));
        }
        Ok(TenantAccess {
            scope,
            permissions: access_token.effective_permissions().into_owned(),
        })
    }

    #[inline]
    pub fn is_server_wide(&self) -> bool {
        self.scope.is_none()
    }

    /// Política de red para las URL que configura: un administrador de inquilino no tiene la posición de red del
    /// servidor (documentación pública).
    pub fn outbound_policy(&self) -> OutboundPolicy {
        if self.is_server_wide() {
            OutboundPolicy::Trusted
        } else {
            OutboundPolicy::PublicOnly
        }
    }

    /// ¿Puede ver este objeto? Los objetos de cuenta los filtra el código libre por `accountId`.
    pub fn can_see(&self, object_type: ObjectType, id: Id, object: &Object) -> bool {
        let Some(scope) = &self.scope else {
            return true;
        };
        if object_type == ObjectType::Tenant {
            // Solo sus descendientes: nunca su propio inquilino ni sus ancestros.
            scope.contains_descendant(id.document_id())
        } else if object_type.flags() & OBJ_FILTER_TENANT != 0 {
            object
                .inner
                .member_tenant_id()
                .is_some_and(|tenant_id| scope.contains(tenant_id.document_id()))
        } else {
            true
        }
    }

    /// Ids visibles de un tipo de objeto, para `get` sin ids y para `query` (`None` = sin restricción).
    pub async fn visible_ids(
        &self,
        server: &Server,
        object_type: ObjectType,
    ) -> trc::Result<Option<RoaringBitmap>> {
        let Some(scope) = &self.scope else {
            return Ok(None);
        };
        if object_type == ObjectType::Tenant {
            Ok(Some(
                scope
                    .ids()
                    .iter()
                    .copied()
                    .filter(|id| *id != scope.root)
                    .collect(),
            ))
        } else if object_type.flags() & OBJ_FILTER_TENANT != 0 {
            server
                .tenant_scope_object_ids(object_type, scope)
                .await
                .map(Some)
        } else {
            Ok(None)
        }
    }

    /// Consulta restringida al ámbito cuando se puede hacer con un solo filtro (un inquilino sin hijos). Si no, la
    /// restricción se aplica después con `visible_ids`.
    pub fn restrict_query(&self, query: RegistryQuery) -> RegistryQuery {
        match &self.scope {
            Some(scope)
                if scope.is_single() && query.object_type().flags() & OBJ_FILTER_TENANT != 0 =>
            {
                query.with_tenant(Some(scope.root))
            }
            _ => query,
        }
    }

    /// ¿Hace falta filtrar los resultados de una consulta después? (revendedores con hijos, o `x:Tenant`).
    pub fn needs_post_filter(&self, object_type: ObjectType) -> bool {
        match &self.scope {
            Some(scope) => {
                object_type == ObjectType::Tenant
                    || (!scope.is_single() && object_type.flags() & OBJ_FILTER_TENANT != 0)
            }
            None => false,
        }
    }

    /// Validaciones de inquilino de un objeto que se crea o se modifica (especificación §4.5, §5 y §8). Rellena
    /// el inquilino (o el padre, para `x:Tenant`) si falta.
    pub async fn validate_write(
        &self,
        server: &Server,
        id: Option<Id>,
        new_object: &mut Object,
        old_object: Option<&Object>,
    ) -> trc::Result<Result<(), SetError<Property>>> {
        let object_type = new_object.object_type();

        // 1. Inquilino (o padre) del objeto.
        if object_type == ObjectType::Tenant {
            if let Err(err) = self
                .validate_tenant(server, id, new_object, old_object)
                .await?
            {
                return Ok(Err(err));
            }
        } else if object_type.flags() & OBJ_FILTER_TENANT != 0 {
            let old_tenant = old_object.and_then(|o| o.inner.member_tenant_id());
            let mut new_tenant = new_object.inner.member_tenant_id();
            if let Some(scope) = &self.scope {
                match new_tenant {
                    None if old_object.is_none() => {
                        new_tenant = Some(Id::from(scope.root));
                        new_object.inner.set_member_tenant_id(new_tenant);
                    }
                    None => {
                        return Ok(Err(SetError::forbidden()
                            .with_property(Property::MemberTenantId)
                            .with_description(
                                "A tenant administrator cannot move objects out of the tenant",
                            )));
                    }
                    Some(tenant_id) if !scope.contains(tenant_id.document_id()) => {
                        return Ok(Err(SetError::forbidden()
                            .with_property(Property::MemberTenantId)
                            .with_description("The tenant is outside your scope")));
                    }
                    Some(_) => {}
                }
            } else if let Some(tenant_id) = new_tenant
                && new_tenant != old_tenant
                && server.tenant(tenant_id.document_id()).await?.is_none()
            {
                return Ok(Err(SetError::new(SetErrorType::InvalidForeignKey)
                    .with_property(Property::MemberTenantId)
                    .with_object_id(ObjectId::new(ObjectType::Tenant, tenant_id))));
            }

            // Al cambiar de inquilino, lo que apunta a este objeto debe ir con él (p. ej. las cuentas de un
            // dominio): si no, quedarían objetos de un inquilino usando objetos de otro.
            if let (Some(id), Some(_)) = (id, old_object)
                && new_tenant != old_tenant
            {
                for linked in server
                    .registry()
                    .linked_objects(ObjectId::new(object_type, id))
                    .await
                    .caused_by(trc::location!())?
                {
                    if linked.object().flags() & OBJ_FILTER_TENANT != 0
                        && server
                            .registry()
                            .get(linked)
                            .await
                            .caused_by(trc::location!())?
                            .and_then(|o| o.inner.member_tenant_id())
                            != new_tenant
                    {
                        return Ok(Err(SetError::new(SetErrorType::ObjectIsLinked)
                            .with_description(
                                "Objects of the current tenant still use this object; move them first",
                            )
                            .with_linked_objects(vec![linked])));
                    }
                }
            }

            // 2. Cuotas de objetos del inquilino al crear o al entrar en él.
            if let Some(tenant_id) = new_tenant
                && new_tenant != old_tenant
                && let Some(quota) = object_quota(&new_object.inner)
                && let Err(exceeded) = server
                    .check_tenant_object_quota(tenant_id.document_id(), quota, 1)
                    .await?
            {
                return Ok(Err(over_quota(exceeded)));
            }
        }

        // 3. Lo que solo puede hacer el operador.
        if self.scope.is_some() {
            // Permisos: no puede dar ninguno que él mismo no tenga (aunque el techo del inquilino los anularía,
            // se rechaza para que quede claro).
            let old = old_object
                .map(|o| requested_permissions(&o.inner))
                .unwrap_or_default();
            if let Some(permission) = requested_permissions(&new_object.inner)
                .into_iter()
                .find(|p| !old.contains(p) && !self.permissions.get(*p as usize))
            {
                return Ok(Err(SetError::forbidden()
                    .with_property(Property::Permissions)
                    .with_description(format!(
                        "You cannot grant the permission '{}' because you do not have it",
                        permission.as_str()
                    ))));
            }

            // Secretos solo por valor: nada de leer archivos o variables de entorno del servidor.
            let value = new_object.clone().into_value();
            if let Some(source) = find_local_source(&value) {
                return Ok(Err(SetError::forbidden().with_description(format!(
                    "Secrets and files must be given by value: '{source}' sources are reserved to the server administrator"
                ))));
            }

            // Claves ajenas: solo a objetos de su ámbito (o servicios compartidos del operador).
            let mut index = IndexBuilder::default();
            new_object.index(&mut index);
            for key in index.keys.iter() {
                if let IndexKey::ForeignKey { object_id, .. } = key
                    && object_id.object().flags() & OBJ_FILTER_TENANT != 0
                    && !self.can_reference(server, object_id).await?
                {
                    return Ok(Err(
                        SetError::new(SetErrorType::InvalidForeignKey).with_object_id(*object_id)
                    ));
                }
            }
        }

        Ok(Ok(()))
    }

    async fn can_reference(&self, server: &Server, object_id: &ObjectId) -> trc::Result<bool> {
        match server
            .registry()
            .get(*object_id)
            .await
            .caused_by(trc::location!())?
        {
            Some(object) => Ok(match object.inner.member_tenant_id() {
                Some(_) => self.can_see(object_id.object(), object_id.id(), &object),
                None => is_shared_service_object(object_id.object()),
            }),
            None => Ok(false),
        }
    }

    /// `x:Tenant`: padre dentro del ámbito, sin ciclos, y un revendedor no toca su propio inquilino.
    async fn validate_tenant(
        &self,
        server: &Server,
        id: Option<Id>,
        new_object: &mut Object,
        old_object: Option<&Object>,
    ) -> trc::Result<Result<(), SetError<Property>>> {
        let ObjectInner::Tenant(tenant) = &mut new_object.inner else {
            return Ok(Ok(()));
        };
        let old_parent = match old_object.map(|o| &o.inner) {
            Some(ObjectInner::Tenant(old)) => old.bezenti_parent_tenant_id,
            _ => None,
        };

        if let Some(scope) = &self.scope {
            match tenant.bezenti_parent_tenant_id {
                None if old_object.is_none() => {
                    tenant.bezenti_parent_tenant_id = Some(Id::from(scope.root));
                }
                None => {
                    return Ok(Err(SetError::forbidden()
                        .with_property(Property::BezentiParentTenantId)
                        .with_description(
                            "A reseller cannot turn a tenant into a top-level tenant",
                        )));
                }
                Some(parent) if !scope.contains(parent.document_id()) => {
                    return Ok(Err(SetError::forbidden()
                        .with_property(Property::BezentiParentTenantId)
                        .with_description("The parent tenant is outside your scope")));
                }
                Some(_) => {}
            }
        }

        if let Some(parent) = tenant.bezenti_parent_tenant_id
            && tenant.bezenti_parent_tenant_id != old_parent
        {
            let parent_id = parent.document_id();
            if server.tenant(parent_id).await?.is_none() {
                return Ok(Err(SetError::new(SetErrorType::InvalidForeignKey)
                    .with_property(Property::BezentiParentTenantId)
                    .with_object_id(ObjectId::new(ObjectType::Tenant, parent))));
            }
            if let Some(id) = id
                && server
                    .tenant_scope(id.document_id())
                    .await?
                    .contains(parent_id)
            {
                return Ok(Err(SetError::invalid_properties()
                    .with_property(Property::BezentiParentTenantId)
                    .with_description(
                        "A tenant cannot be its own parent or the child of one of its descendants",
                    )));
            }
        }

        if tenant.name.trim().is_empty() {
            return Ok(Err(SetError::invalid_properties()
                .with_property(Property::Name)
                .with_description("The tenant name cannot be empty")));
        }

        Ok(Ok(()))
    }
}

/// Quita de un error lo que podría revelar objetos de otro inquilino (el id del objeto con el que choca una
/// clave única, p. ej. un dominio de otro inquilino con el mismo nombre).
pub(crate) fn sanitize_set_error(err: &mut SetError<Property>) {
    if matches!(err.error_type(), SetErrorType::PrimaryKeyViolation) {
        *err = std::mem::replace(err, SetError::forbidden()).with_object_id_opt(None);
    }
}

/// Error `overQuota` con el texto de la documentación.
pub(crate) fn over_quota(exceeded: TenantQuotaExceeded) -> SetError<Property> {
    SetError::new(SetErrorType::OverQuota).with_description(exceeded.description())
}

/// Permisos que un objeto concede explícitamente (cuentas, roles e inquilinos).
fn requested_permissions(object: &ObjectInner) -> Vec<Permission> {
    let from = |permissions: &structs::Permissions| match permissions {
        structs::Permissions::Inherit => Vec::new(),
        structs::Permissions::Merge(list) | structs::Permissions::Replace(list) => {
            list.enabled_permissions.as_slice().to_vec()
        }
    };
    match object {
        ObjectInner::Account(account) => match account.as_ref() {
            Account::User(account) => from(&account.permissions),
            Account::Group(account) => from(&account.permissions),
        },
        ObjectInner::Role(role) => role.enabled_permissions.as_slice().to_vec(),
        ObjectInner::Tenant(tenant) => from(&tenant.permissions),
        _ => Vec::new(),
    }
}

/// Busca en un objeto una variante que lea del sistema de archivos o del entorno del servidor.
fn find_local_source(value: &JmapValue<'_>) -> Option<&'static str> {
    match value {
        JmapValue::Object(obj) => {
            if let Some(JmapValue::Str(typ)) = obj.get(&Key::Property(Property::Type)) {
                match typ.as_ref() {
                    "File" => return Some("File"),
                    "EnvironmentVariable" => return Some("EnvironmentVariable"),
                    _ => {}
                }
            }
            obj.iter().find_map(|(_, value)| find_local_source(value))
        }
        JmapValue::Array(values) => values.iter().find_map(find_local_source),
        _ => None,
    }
}

/// `usedDiskQuota` de un inquilino (calculado: la suma del uso de sus cuentas y descendientes).
pub(crate) async fn tenant_used_disk(server: &Server, id: Id) -> trc::Result<u64> {
    server.tenant_used_disk(id.document_id()).await
}
