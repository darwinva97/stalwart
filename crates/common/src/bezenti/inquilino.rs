/*
 * SPDX-FileCopyrightText: 2026 bezenti
 *
 * SPDX-License-Identifier: AGPL-3.0-only
 */

//! Multi-inquilino y revendedores (edición bezenti, fase 2).
//!
//! Especificación: `bezenti/especificaciones/multi-inquilino.md`. Resumen de lo que hay aquí:
//!
//! - **Caché de inquilinos** (`Server::tenant`): nombre, padre (`bezentiParentTenantId`), techo de permisos ya
//!   calculado (incluida la intersección con el techo de sus ancestros), cuotas y logo.
//! - **Ámbito** (`Server::tenant_scope`): un inquilino y todos sus descendientes. Es lo que ve y gestiona su
//!   administrador.
//! - **Permisos efectivos** de una cuenta con inquilino (`Server::account_permissions`): roles, campo
//!   `permissions` de la cuenta, techo del inquilino y sin `impersonate`.
//! - **Cuotas** del inquilino: número de objetos y disco, jerárquicas (lo de un hijo cuenta para el padre).

use crate::{
    Server,
    auth::{AccessToken, Permissions, PermissionsGroup},
};
use registry::{
    schema::{
        enums::{AccountType, Permission, TenantStorageQuota},
        prelude::{OBJ_FILTER_TENANT, ObjectInner, ObjectType, Property},
        structs::{self, Account, Roles, Tenant, UserRoles},
    },
    types::EnumImpl,
};
use std::{future::Future, pin::Pin, sync::Arc, time::Duration};
use store::{registry::RegistryQuery, roaring::RoaringBitmap};
use trc::{AddContext, CacheEvent};
use types::id::Id;
use utils::{cache::CacheItemWeight, map::vec_map::VecMap};

/// Profundidad máxima de la jerarquía de revendedores (defensa contra ciclos en datos corruptos).
pub const MAX_TENANT_DEPTH: usize = 16;

/// Cuánto se guarda en caché el uso de disco de un inquilino (la suma de sus cuentas).
pub const TENANT_DISK_USAGE_TTL: Duration = Duration::from_secs(10);

/// Marca que se pone (clave `type`) en un error `limit.quota` cuando lo que se ha superado es la cuota de disco
/// del inquilino (y no la de la cuenta). Sirve para dar el mensaje de la documentación en IMAP y SMTP.
pub const TENANT_QUOTA_DETAILS: &str = "tenant";

/// Datos de un inquilino ya preparados para usar en cada petición.
#[derive(Debug, Clone)]
pub struct TenantCache {
    pub id: u32,
    pub name: Box<str>,
    /// Inquilino padre (revendedor), si lo tiene.
    pub parent_id: Option<u32>,
    /// Permisos máximos que pueden tener sus miembros (ya cortados por el techo de sus ancestros).
    pub ceiling: Permissions,
    pub quotas: VecMap<TenantStorageQuota, u64>,
    pub logo: Option<Box<str>>,
    /// Fase 8: máximo de mensajes en la cola de salida (`bezentiMaxQueuedMessages`).
    pub max_queued_messages: Option<u64>,
    /// Fase 8: mensajes por intervalo (`bezentiMessageRate`).
    pub message_rate: Option<structs::Rate>,
    /// Fase 8: destinatarios por intervalo (`bezentiRecipientRate`).
    pub recipient_rate: Option<structs::Rate>,
}

impl CacheItemWeight for TenantCache {
    fn weight(&self) -> u64 {
        (std::mem::size_of::<TenantCache>()
            + self.name.len()
            + self.logo.as_ref().map_or(0, |l| l.len())
            + self.quotas.len() * std::mem::size_of::<(TenantStorageQuota, u64)>()) as u64
    }
}

impl TenantCache {
    /// Límite de una cuota (`None` = sin límite; un 0 también se toma como «sin límite»).
    pub fn quota(&self, quota: TenantStorageQuota) -> Option<u64> {
        self.quotas.get(&quota).copied().filter(|limit| *limit > 0)
    }
}

/// Un inquilino y todos sus descendientes: lo que ve y gestiona un administrador de ese inquilino.
#[derive(Debug, Clone)]
pub struct TenantScope {
    pub root: u32,
    /// Ids ordenados (incluye `root`).
    ids: Vec<u32>,
}

impl TenantScope {
    pub fn contains(&self, tenant_id: u32) -> bool {
        self.ids.binary_search(&tenant_id).is_ok()
    }

    /// ¿Está el inquilino dentro del ámbito pero sin ser la raíz? (los inquilinos que un revendedor gestiona).
    pub fn contains_descendant(&self, tenant_id: u32) -> bool {
        tenant_id != self.root && self.contains(tenant_id)
    }

    pub fn ids(&self) -> &[u32] {
        &self.ids
    }

    pub fn is_single(&self) -> bool {
        self.ids.len() == 1
    }
}

/// Qué objetos cuenta cada cuota del inquilino: tipo de objeto y, para las cuentas, el tipo de cuenta.
pub fn quota_object(quota: TenantStorageQuota) -> Option<(ObjectType, Option<AccountType>)> {
    Some(match quota {
        TenantStorageQuota::MaxAccounts => (ObjectType::Account, Some(AccountType::User)),
        TenantStorageQuota::MaxGroups => (ObjectType::Account, Some(AccountType::Group)),
        TenantStorageQuota::MaxDomains => (ObjectType::Domain, None),
        TenantStorageQuota::MaxMailingLists => (ObjectType::MailingList, None),
        TenantStorageQuota::MaxRoles => (ObjectType::Role, None),
        TenantStorageQuota::MaxOauthClients => (ObjectType::OAuthClient, None),
        TenantStorageQuota::MaxDkimKeys => (ObjectType::DkimSignature, None),
        TenantStorageQuota::MaxDnsServers => (ObjectType::DnsServer, None),
        TenantStorageQuota::MaxDirectories => (ObjectType::Directory, None),
        TenantStorageQuota::MaxAcmeProviders => (ObjectType::AcmeProvider, None),
        TenantStorageQuota::MaxDiskQuota => return None,
    })
}

/// La cuota de inquilino que gasta un objeto al crearlo (o al entrar en el inquilino).
pub fn object_quota(object: &ObjectInner) -> Option<TenantStorageQuota> {
    Some(match object {
        ObjectInner::Account(account) => match account.as_ref() {
            Account::User(_) => TenantStorageQuota::MaxAccounts,
            Account::Group(_) => TenantStorageQuota::MaxGroups,
        },
        ObjectInner::Domain(_) => TenantStorageQuota::MaxDomains,
        ObjectInner::MailingList(_) => TenantStorageQuota::MaxMailingLists,
        ObjectInner::Role(_) => TenantStorageQuota::MaxRoles,
        ObjectInner::OAuthClient(_) => TenantStorageQuota::MaxOauthClients,
        ObjectInner::DkimSignature(_) => TenantStorageQuota::MaxDkimKeys,
        ObjectInner::DnsServer(_) => TenantStorageQuota::MaxDnsServers,
        ObjectInner::Directory(_) => TenantStorageQuota::MaxDirectories,
        ObjectInner::AcmeProvider(_) => TenantStorageQuota::MaxAcmeProviders,
        _ => return None,
    })
}

/// Nombre en plural de lo que cuenta una cuota (para el mensaje de error de la documentación).
pub fn quota_noun(quota: TenantStorageQuota) -> &'static str {
    match quota {
        TenantStorageQuota::MaxAccounts => "accounts",
        TenantStorageQuota::MaxGroups => "groups",
        TenantStorageQuota::MaxDomains => "domains",
        TenantStorageQuota::MaxMailingLists => "mailing lists",
        TenantStorageQuota::MaxRoles => "roles",
        TenantStorageQuota::MaxOauthClients => "OAuth clients",
        TenantStorageQuota::MaxDkimKeys => "DKIM keys",
        TenantStorageQuota::MaxDnsServers => "DNS servers",
        TenantStorageQuota::MaxDirectories => "directories",
        TenantStorageQuota::MaxAcmeProviders => "ACME providers",
        TenantStorageQuota::MaxDiskQuota => "bytes",
    }
}

/// Error de cuota de un inquilino: qué cuota y cuál es su límite.
#[derive(Debug, Clone, Copy)]
pub struct TenantQuotaExceeded {
    pub tenant_id: u32,
    pub quota: TenantStorageQuota,
    pub limit: u64,
}

impl TenantQuotaExceeded {
    /// Texto del error `overQuota` (el de la documentación: «You have exceeded your quota of 50 accounts»).
    pub fn description(&self) -> String {
        format!(
            "You have exceeded your quota of {} {}",
            self.limit,
            quota_noun(self.quota)
        )
    }
}

/// ¿Es este error el de la cuota de disco de un inquilino? (ver `TENANT_QUOTA_DETAILS`).
pub fn is_tenant_quota_error(err: &trc::Error) -> bool {
    err.matches(trc::EventType::Limit(trc::LimitEvent::Quota))
        && err.value_as_str(trc::Key::Type) == Some(TENANT_QUOTA_DETAILS)
}

impl AccessToken {
    /// ¿Puede quien hace la petición ver y gestionar todo el servidor? (sin inquilino).
    #[inline(always)]
    pub fn is_server_wide(&self) -> bool {
        self.tenant_id().is_none()
    }
}

/// Tipos de objeto del registro que un usuario de inquilino puede usar (ver la especificación, §4.2): los de
/// inquilino, los de su propia cuenta y `x:Tenant` (solo descendientes).
pub fn is_tenant_accessible_object(object_type: ObjectType) -> bool {
    use registry::schema::prelude::OBJ_FILTER_ACCOUNT;
    let flags = object_type.flags();
    flags & (OBJ_FILTER_TENANT | OBJ_FILTER_ACCOUNT) != 0
        || matches!(
            object_type,
            ObjectType::Tenant
                | ObjectType::AccountSettings
                | ObjectType::ApiKey
                | ObjectType::AccountPassword
                | ObjectType::AppPassword
                | ObjectType::Passkey
        )
}

/// Tipos de objeto del operador que un administrador de inquilino puede usar como clave ajena en sus objetos
/// (servicios compartidos que no exponen datos de nadie).
pub fn is_shared_service_object(object_type: ObjectType) -> bool {
    matches!(
        object_type,
        ObjectType::AcmeProvider | ObjectType::DnsServer
    )
}

type BoxFuture<'x, T> = Pin<Box<dyn Future<Output = T> + Send + 'x>>;

impl Server {
    /// Datos de un inquilino (de la caché o del registro). `None` si no existe.
    pub async fn tenant(&self, tenant_id: u32) -> trc::Result<Option<Arc<TenantCache>>> {
        self.tenant_with_depth(tenant_id, 0).await
    }

    fn tenant_with_depth(
        &self,
        tenant_id: u32,
        depth: usize,
    ) -> BoxFuture<'_, trc::Result<Option<Arc<TenantCache>>>> {
        Box::pin(async move {
            let cache = &self.inner.cache.tenants;
            match cache.get_value_or_guard_async(&tenant_id).await {
                Ok(tenant) => {
                    trc::event!(Cache(CacheEvent::Hit), Key = tenant_id, Name = "tenant");
                    Ok(Some(tenant))
                }
                Err(guard) => {
                    trc::event!(Cache(CacheEvent::Miss), Key = tenant_id, Name = "tenant");
                    let Some(tenant) = self
                        .registry()
                        .object::<Tenant>(Id::from(tenant_id))
                        .await
                        .caused_by(trc::location!())?
                    else {
                        return Ok(None);
                    };

                    // Techo propio: sus roles (Default = defaultTenantRoleIds) y su campo `permissions`.
                    let role_ids: Vec<Id> = match &tenant.roles {
                        Roles::Default => {
                            self.core.network.security.default_role_ids_tenant.clone()
                        }
                        Roles::Custom(custom) => custom.role_ids.as_slice().to_vec(),
                    };
                    let mut ceiling = self
                        .effective_permissions(&tenant.permissions, &role_ids)
                        .await
                        .caused_by(trc::location!())?
                        .finalize();
                    ceiling.clear(Permission::Impersonate as usize);

                    // Techo del padre: un revendedor no puede dar más de lo que tiene.
                    let parent_id = tenant
                        .bezenti_parent_tenant_id
                        .map(|id| id.document_id())
                        .filter(|id| *id != tenant_id);
                    if let Some(parent_id) = parent_id {
                        match (depth < MAX_TENANT_DEPTH)
                            .then(|| self.tenant_with_depth(parent_id, depth + 1))
                        {
                            Some(parent) => match parent.await? {
                                Some(parent) => ceiling.intersection(&parent.ceiling),
                                None => ceiling.clear_all(),
                            },
                            None => ceiling.clear_all(),
                        }
                    }

                    let cached = Arc::new(TenantCache {
                        id: tenant_id,
                        name: tenant.name.into_boxed_str(),
                        parent_id,
                        ceiling,
                        quotas: tenant.quotas,
                        logo: tenant.logo.map(|l| l.into_boxed_str()),
                        max_queued_messages: tenant.bezenti_max_queued_messages.filter(|m| *m > 0),
                        message_rate: tenant.bezenti_message_rate.filter(|r| r.count > 0),
                        recipient_rate: tenant.bezenti_recipient_rate.filter(|r| r.count > 0),
                    });
                    let _ = guard.insert(cached.clone());
                    Ok(Some(cached))
                }
            }
        })
    }

    /// Hijos directos de un inquilino (por el índice de `bezentiParentTenantId`).
    pub async fn tenant_children(&self, tenant_id: u32) -> trc::Result<RoaringBitmap> {
        self.registry()
            .query::<RoaringBitmap>(
                RegistryQuery::new(ObjectType::Tenant)
                    .equal(Property::BezentiParentTenantId, tenant_id as u64),
            )
            .await
            .caused_by(trc::location!())
    }

    /// Un inquilino y todos sus descendientes.
    pub async fn tenant_scope(&self, tenant_id: u32) -> trc::Result<TenantScope> {
        let mut ids = vec![tenant_id];
        let mut pending = vec![(tenant_id, 0usize)];
        while let Some((id, depth)) = pending.pop() {
            if depth >= MAX_TENANT_DEPTH {
                continue;
            }
            for child in self.tenant_children(id).await? {
                if !ids.contains(&child) {
                    ids.push(child);
                    pending.push((child, depth + 1));
                }
            }
        }
        ids.sort_unstable();
        Ok(TenantScope {
            root: tenant_id,
            ids,
        })
    }

    /// Ámbito de quien hace la petición: `None` si es del operador (ve todo).
    pub async fn access_token_scope(
        &self,
        access_token: &AccessToken,
    ) -> trc::Result<Option<TenantScope>> {
        match access_token.tenant_id() {
            Some(tenant_id) => self.tenant_scope(tenant_id).await.map(Some),
            None => Ok(None),
        }
    }

    /// Ancestros de un inquilino, empezando por él mismo (`[t, padre, abuelo, ...]`).
    pub async fn tenant_ancestors(&self, tenant_id: u32) -> trc::Result<Vec<Arc<TenantCache>>> {
        let mut chain: Vec<Arc<TenantCache>> = Vec::new();
        let mut next = Some(tenant_id);
        while let Some(id) = next {
            if chain.len() > MAX_TENANT_DEPTH || chain.iter().any(|t| t.id == id) {
                break;
            }
            match self.tenant(id).await? {
                Some(tenant) => {
                    next = tenant.parent_id;
                    chain.push(tenant);
                }
                None => break,
            }
        }
        Ok(chain)
    }

    /// Ids de los objetos de un tipo de inquilino que están dentro de un ámbito.
    pub async fn tenant_scope_object_ids(
        &self,
        object_type: ObjectType,
        scope: &TenantScope,
    ) -> trc::Result<RoaringBitmap> {
        let mut ids = RoaringBitmap::new();
        for tenant_id in scope.ids() {
            ids |= self
                .registry()
                .query::<RoaringBitmap>(
                    RegistryQuery::new(object_type).with_tenant(Some(*tenant_id)),
                )
                .await
                .caused_by(trc::location!())?;
        }
        Ok(ids)
    }

    /// Permisos efectivos de una cuenta teniendo en cuenta su inquilino (especificación, §3):
    /// roles → campo `permissions` → techo del inquilino → sin `impersonate`.
    pub async fn account_permissions(&self, account: &Account) -> trc::Result<PermissionsGroup> {
        let security = &self.core.network.security;
        let tenant_id = match account {
            Account::User(account) => account.member_tenant_id,
            Account::Group(account) => account.member_tenant_id,
        };
        let tenant = match tenant_id {
            Some(tenant_id) => Some(
                self.tenant(tenant_id.document_id())
                    .await?
                    .map(|tenant| tenant.ceiling.clone())
                    // Un inquilino que ya no existe no concede nada.
                    .unwrap_or_default(),
            ),
            None => None,
        };

        let mut permissions = match account {
            Account::User(account) => match (&account.roles, &tenant) {
                // Administrador de inquilino: todo lo que el inquilino puede hacer y nada más.
                (UserRoles::Admin, Some(ceiling)) => apply_account_permissions(
                    PermissionsGroup {
                        enabled: ceiling.clone(),
                        ..Default::default()
                    },
                    &account.permissions,
                ),
                (roles, _) => {
                    let role_ids = match roles {
                        UserRoles::User => security.default_role_ids_user.as_slice(),
                        UserRoles::Admin => security.default_role_ids_admin.as_slice(),
                        UserRoles::Custom(custom) => custom.role_ids.as_slice(),
                    };
                    self.effective_permissions(&account.permissions, role_ids)
                        .await?
                }
            },
            Account::Group(account) => {
                let role_ids = account
                    .roles
                    .role_ids()
                    .unwrap_or(security.default_role_ids_group.as_slice());
                self.effective_permissions(&account.permissions, role_ids)
                    .await?
            }
        };

        if let Some(ceiling) = tenant {
            permissions.enabled.intersection(&ceiling);
            permissions.disabled.set(Permission::Impersonate as usize);
        }

        Ok(permissions)
    }

    /// Cuántos objetos que gastan la cuota `quota` hay en un ámbito (un inquilino y sus descendientes).
    pub async fn tenant_scope_object_count(
        &self,
        scope: &TenantScope,
        quota: TenantStorageQuota,
    ) -> trc::Result<u64> {
        let Some((object_type, account_type)) = quota_object(quota) else {
            return Ok(0);
        };
        let mut total = 0u64;
        for tenant_id in scope.ids() {
            let mut query = RegistryQuery::new(object_type).with_tenant(Some(*tenant_id));
            if let Some(account_type) = account_type {
                query = query.equal(Property::Type, account_type.to_id());
            }
            // (RegistryObjectCounter no sabe cruzar dos filtros; un mapa de bits sí)
            total += self
                .registry()
                .query::<RoaringBitmap>(query)
                .await
                .caused_by(trc::location!())?
                .len();
        }
        Ok(total)
    }

    /// Comprueba las cuotas de objetos al meter `added` objetos nuevos en el inquilino `tenant_id`, para él y
    /// para cada uno de sus ancestros (cuotas jerárquicas).
    pub async fn check_tenant_object_quota(
        &self,
        tenant_id: u32,
        quota: TenantStorageQuota,
        added: u64,
    ) -> trc::Result<Result<(), TenantQuotaExceeded>> {
        for tenant in self.tenant_ancestors(tenant_id).await? {
            if let Some(limit) = tenant.quota(quota) {
                let scope = self.tenant_scope(tenant.id).await?;
                let used = self.tenant_scope_object_count(&scope, quota).await?;
                if used.saturating_add(added) > limit {
                    return Ok(Err(TenantQuotaExceeded {
                        tenant_id: tenant.id,
                        quota,
                        limit,
                    }));
                }
            }
        }
        Ok(Ok(()))
    }

    /// Espacio en disco usado por un inquilino y sus descendientes: la suma del uso de sus cuentas.
    /// Se guarda en caché `TENANT_DISK_USAGE_TTL`.
    pub async fn tenant_used_disk(&self, tenant_id: u32) -> trc::Result<u64> {
        let cache = &self.inner.cache.tenant_disk_usage;
        if let Some(used) = cache.get(&tenant_id) {
            return Ok(used);
        }
        let scope = self.tenant_scope(tenant_id).await?;
        let mut used = 0u64;
        for account_id in self
            .tenant_scope_object_ids(ObjectType::Account, &scope)
            .await?
        {
            used =
                used.saturating_add(self.get_used_quota_account(account_id).await?.max(0) as u64);
        }
        cache.insert(tenant_id, used, TENANT_DISK_USAGE_TTL);
        Ok(used)
    }

    /// Espacio libre que le queda a una cuenta del inquilino `tenant_id` según las cuotas de disco del inquilino
    /// y de sus ancestros (`None` = sin límite de inquilino).
    pub async fn tenant_disk_available(&self, tenant_id: u32) -> trc::Result<Option<u64>> {
        let mut available: Option<u64> = None;
        for tenant in self.tenant_ancestors(tenant_id).await? {
            if let Some(limit) = tenant.quota(TenantStorageQuota::MaxDiskQuota) {
                let free = limit.saturating_sub(self.tenant_used_disk(tenant.id).await?);
                available = Some(available.map_or(free, |a| a.min(free)));
            }
        }
        Ok(available)
    }

    /// Error `limit.quota` (con la marca de inquilino) si `item_size` bytes no caben en la cuota de disco del
    /// inquilino o de alguno de sus ancestros.
    pub async fn check_tenant_disk_quota(&self, tenant_id: u32, item_size: u64) -> trc::Result<()> {
        if let Some(available) = self.tenant_disk_available(tenant_id).await?
            && item_size > available
        {
            return Err(trc::LimitEvent::Quota
                .into_err()
                .ctx(trc::Key::Type, TENANT_QUOTA_DETAILS)
                .ctx(trc::Key::Id, tenant_id)
                .ctx(trc::Key::Limit, available)
                .ctx(trc::Key::Size, item_size));
        }
        Ok(())
    }

    /// Olvida los datos de inquilino guardados en caché (al cambiar un inquilino o sus roles).
    pub fn invalidate_tenant_caches(&self) {
        self.inner.cache.tenants.clear();
        self.inner.cache.tenant_disk_usage.clear();
    }
}

fn apply_account_permissions(
    mut base: PermissionsGroup,
    permissions: &structs::Permissions,
) -> PermissionsGroup {
    match permissions {
        structs::Permissions::Inherit => base,
        structs::Permissions::Merge(list) => {
            base.union(&PermissionsGroup::from(list));
            base
        }
        structs::Permissions::Replace(list) => PermissionsGroup::from(list),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ambito_y_descendientes() {
        let scope = TenantScope {
            root: 5,
            ids: vec![5, 7, 9],
        };
        assert!(scope.contains(5) && scope.contains(9) && !scope.contains(6));
        assert!(
            !scope.contains_descendant(5),
            "un inquilino no se gestiona a sí mismo"
        );
        assert!(scope.contains_descendant(7));
        assert!(!scope.is_single());
    }

    #[test]
    fn cada_cuota_cuenta_su_tipo_de_objeto() {
        assert_eq!(
            quota_object(TenantStorageQuota::MaxAccounts),
            Some((ObjectType::Account, Some(AccountType::User)))
        );
        assert_eq!(
            quota_object(TenantStorageQuota::MaxGroups),
            Some((ObjectType::Account, Some(AccountType::Group)))
        );
        assert_eq!(quota_object(TenantStorageQuota::MaxDiskQuota), None);
        for quota in (0..TenantStorageQuota::COUNT as u16).filter_map(TenantStorageQuota::from_id) {
            if let Some((object_type, _)) = quota_object(quota) {
                assert!(
                    object_type.flags() & OBJ_FILTER_TENANT != 0,
                    "{object_type:?} debe ser un objeto de inquilino"
                );
            }
        }
    }

    #[test]
    fn mensaje_de_cuota_de_la_documentacion() {
        let err = TenantQuotaExceeded {
            tenant_id: 1,
            quota: TenantStorageQuota::MaxAccounts,
            limit: 50,
        };
        assert_eq!(
            err.description(),
            "You have exceeded your quota of 50 accounts"
        );
    }

    #[test]
    fn marca_de_cuota_de_inquilino() {
        let tenant = trc::LimitEvent::Quota
            .into_err()
            .ctx(trc::Key::Type, TENANT_QUOTA_DETAILS);
        assert!(is_tenant_quota_error(&tenant));
        assert!(!is_tenant_quota_error(&trc::LimitEvent::Quota.into_err()));
    }
}
