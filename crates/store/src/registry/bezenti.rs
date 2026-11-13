/*
 * SPDX-FileCopyrightText: 2026 bezenti
 *
 * SPDX-License-Identifier: AGPL-3.0-only
 */

//! Multi-inquilino en el almacén del registro (edición bezenti, fase 2).
//!
//! Dos piezas pequeñas que usan las capas de arriba (ver `bezenti/especificaciones/multi-inquilino.md`):
//!
//! - `RegistryQuery::with_tenant`: restringe una consulta a los objetos de un inquilino (`memberTenantId`).
//! - `RegistryStore::tenant_foreign_key_in_scope`: la red de seguridad al escribir. Un objeto de un inquilino
//!   solo puede apuntar (clave ajena) a objetos de su mismo inquilino o a objetos sin inquilino; un objeto sin
//!   inquilino no puede apuntar a objetos de un inquilino. Así ninguna ruta de escritura (API, tareas, consola)
//!   puede mezclar datos de dos inquilinos.

use crate::{
    RegistryStore,
    registry::{RegistryFilter, RegistryQuery},
};
use registry::{
    schema::prelude::{OBJ_FILTER_TENANT, ObjectType, Property},
    types::id::ObjectId,
};
use trc::AddContext;
use types::id::Id;

impl RegistryQuery {
    /// Limita la consulta a los objetos del inquilino `tenant_id` (solo si el tipo de objeto es de inquilino).
    pub fn with_tenant(mut self, tenant_id: Option<u32>) -> Self {
        if let Some(tenant_id) = tenant_id
            && self.object_type.flags() & OBJ_FILTER_TENANT != 0
        {
            let filter = RegistryFilter::equal(Property::MemberTenantId, tenant_id as u64, false);
            if self.filters.is_empty() {
                self.filters.push(filter);
            } else {
                self.filters.insert(0, filter);
            }
        }
        self
    }

    pub fn object_type(&self) -> ObjectType {
        self.object_type
    }
}

impl RegistryStore {
    /// ¿Puede un objeto de tipo `object_type` del inquilino `tenant_id` apuntar a `foreign_id`?
    ///
    /// Solo se comprueba entre objetos de inquilino (`OBJ_FILTER_TENANT`) en los dos extremos.
    pub async fn tenant_foreign_key_in_scope(
        &self,
        object_type: ObjectType,
        tenant_id: Option<Id>,
        foreign_id: &ObjectId,
    ) -> trc::Result<bool> {
        if object_type.flags() & OBJ_FILTER_TENANT == 0
            || foreign_id.object().flags() & OBJ_FILTER_TENANT == 0
        {
            return Ok(true);
        }
        let target_tenant = self
            .get(*foreign_id)
            .await
            .caused_by(trc::location!())?
            .and_then(|object| object.inner.member_tenant_id());
        Ok(match (tenant_id, target_tenant) {
            // Mismo inquilino, o los dos sin inquilino.
            (a, b) if a == b => true,
            // Un objeto de inquilino puede usar objetos compartidos del operador (p. ej. un proveedor ACME).
            (Some(_), None) => true,
            // Un objeto sin inquilino no puede apuntar a uno de un inquilino, ni uno de un inquilino a otro.
            _ => false,
        })
    }
}
