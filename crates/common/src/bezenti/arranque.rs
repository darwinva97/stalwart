/*
 * SPDX-FileCopyrightText: 2026 bezenti
 *
 * SPDX-License-Identifier: AGPL-3.0-only
 */

//! Valores por defecto de la edición bezenti que se crean al arrancar.
//!
//! Fase 2: el rol «Tenant Administrator» y `x:Authentication.defaultTenantRoleIds` (techo por defecto de los
//! inquilinos y rol de sus administradores). La documentación pública dice que en el primer arranque se crean los
//! roles User, Group, Tenant Administrator y System Administrator, y que `defaultTenantRoleIds` lleva Tenant
//! Administrator junto con User. Aquí se crea si falta, también en instalaciones que ya existían antes de la
//! edición bezenti (que tienen los otros roles pero no este).

use crate::auth::permissions::DefaultPermissions;
use registry::{
    schema::{
        prelude::{Object, ObjectType},
        structs::{Authentication, Role},
    },
    types::map::Map,
};
use store::registry::{
    bootstrap::Bootstrap,
    write::{RegistryWrite, RegistryWriteResult},
};
use types::id::Id;

pub(crate) const TENANT_ADMIN_ROLE: &str = "Tenant Administrator";

pub(crate) async fn ensure_tenant_admin_role(bp: &mut Bootstrap) -> trc::Result<()> {
    let object_id = ObjectType::Authentication.singleton();
    let Some(old_object) = bp.registry.get(object_id).await? else {
        // Sin x:Authentication todavía no hay roles por defecto: se hará en el siguiente arranque.
        return Ok(());
    };
    let mut authentication = Authentication::from(old_object.clone());
    if !authentication.default_tenant_role_ids.is_empty()
        || authentication.default_user_role_ids.is_empty()
    {
        return Ok(());
    }

    let role = Role {
        description: TENANT_ADMIN_ROLE.into(),
        enabled_permissions: Map::new(DefaultPermissions::default().tenant_admin),
        ..Default::default()
    };
    let role_id = match bp
        .registry
        .write(RegistryWrite::insert(&role.into()))
        .await?
    {
        RegistryWriteResult::Success(id) => id,
        err => {
            bp.build_error(
                ObjectType::Role.singleton(),
                format!("Failed to insert the default tenant administrator role: {err}"),
            );
            return Ok(());
        }
    };

    let mut role_ids: Vec<Id> = vec![role_id];
    role_ids.extend(authentication.default_user_role_ids.iter().copied());
    authentication.default_tenant_role_ids = Map::new(role_ids);
    let new_object = Object {
        inner: authentication.into(),
        revision: old_object.revision,
    };
    let result = bp
        .registry
        .write(RegistryWrite::update(
            Id::singleton(),
            &new_object,
            &old_object,
        ))
        .await?;
    if !matches!(result, RegistryWriteResult::Success(_)) {
        bp.build_error(
            object_id,
            format!("Failed to assign the default tenant roles: {result}"),
        );
    }
    Ok(())
}
