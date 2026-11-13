/*
 * SPDX-FileCopyrightText: 2026 bezenti
 *
 * SPDX-License-Identifier: AGPL-3.0-only
 */

//! Edición bezenti, fase 7: eventos `scim.*` del aprovisionamiento SCIM 2.0.
//!
//! Los nombres y las etiquetas vienen del esquema público (`resources/schema/schema.json.gz`, enum `EventType`) y
//! los niveles de la documentación pública («Diagnosing failures»). Los identificadores numéricos son decisión
//! bezenti: huecos libres de la numeración del código limpio (615–624, 349 y 350), para no mover ninguno de los
//! existentes. Ver `bezenti/especificaciones/scim.md`, §13.

use crate::{EventType, Level};

/// Eventos `scim.*` (esquema, enum `EventType`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u16)]
pub enum ScimEvent {
    /// `scim.request`: petición atendida (método, ruta, estado y tiempo).
    Request = 615,
    /// `scim.forbidden`: respuesta 403.
    Forbidden = 616,
    /// `scim.conflict`: respuesta 409 o 412.
    Conflict = 617,
    /// `scim.error`: cualquier otro error.
    Error = 618,
    /// `scim.resource-created`: usuario o grupo creado.
    ResourceCreated = 619,
    /// `scim.resource-updated`: usuario o grupo cambiado.
    ResourceUpdated = 620,
    /// `scim.resource-deleted`: usuario o grupo borrado.
    ResourceDeleted = 621,
    /// `scim.query`: lista o búsqueda evaluada.
    Query = 622,
    /// `scim.attribute-ignored`: un PATCH apuntó a un atributo que se acepta pero no se guarda.
    AttributeIgnored = 623,
    /// `scim.bulk-processed`: petición Bulk procesada.
    BulkProcessed = 624,
    /// `scim.bulk-aborted`: Bulk detenida al llegar a `failOnErrors`.
    BulkAborted = 349,
    /// `scim.rollback-failed`: no se pudo deshacer un cambio que falló a medias.
    RollbackFailed = 350,
}

pub(crate) const SCIM_EVENTS: [ScimEvent; 12] = [
    ScimEvent::Request,
    ScimEvent::Forbidden,
    ScimEvent::Conflict,
    ScimEvent::Error,
    ScimEvent::ResourceCreated,
    ScimEvent::ResourceUpdated,
    ScimEvent::ResourceDeleted,
    ScimEvent::Query,
    ScimEvent::AttributeIgnored,
    ScimEvent::BulkProcessed,
    ScimEvent::BulkAborted,
    ScimEvent::RollbackFailed,
];

impl ScimEvent {
    pub const fn to_id(&self) -> u16 {
        *self as u16
    }

    pub fn from_id(id: u16) -> Option<Self> {
        SCIM_EVENTS.iter().copied().find(|e| e.to_id() == id)
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            ScimEvent::Request => "scim.request",
            ScimEvent::Forbidden => "scim.forbidden",
            ScimEvent::Conflict => "scim.conflict",
            ScimEvent::Error => "scim.error",
            ScimEvent::ResourceCreated => "scim.resource-created",
            ScimEvent::ResourceUpdated => "scim.resource-updated",
            ScimEvent::ResourceDeleted => "scim.resource-deleted",
            ScimEvent::Query => "scim.query",
            ScimEvent::AttributeIgnored => "scim.attribute-ignored",
            ScimEvent::BulkProcessed => "scim.bulk-processed",
            ScimEvent::BulkAborted => "scim.bulk-aborted",
            ScimEvent::RollbackFailed => "scim.rollback-failed",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        SCIM_EVENTS.iter().copied().find(|e| e.as_str() == value)
    }

    pub fn level(&self) -> Level {
        match self {
            ScimEvent::Request
            | ScimEvent::Error
            | ScimEvent::Query
            | ScimEvent::AttributeIgnored
            | ScimEvent::BulkProcessed => Level::Debug,
            ScimEvent::Forbidden
            | ScimEvent::Conflict
            | ScimEvent::ResourceCreated
            | ScimEvent::ResourceUpdated
            | ScimEvent::ResourceDeleted => Level::Info,
            ScimEvent::BulkAborted => Level::Warn,
            ScimEvent::RollbackFailed => Level::Error,
        }
    }

    pub fn description(&self) -> &'static str {
        match self {
            ScimEvent::Request => "SCIM request handled",
            ScimEvent::Forbidden => "SCIM operation forbidden",
            ScimEvent::Conflict => "SCIM resource conflict",
            ScimEvent::Error => "SCIM request failed",
            ScimEvent::ResourceCreated => "SCIM resource created",
            ScimEvent::ResourceUpdated => "SCIM resource updated",
            ScimEvent::ResourceDeleted => "SCIM resource deleted",
            ScimEvent::Query => "SCIM query executed",
            ScimEvent::AttributeIgnored => "SCIM attribute ignored",
            ScimEvent::BulkProcessed => "SCIM bulk request processed",
            ScimEvent::BulkAborted => "SCIM bulk request aborted",
            ScimEvent::RollbackFailed => "SCIM rollback failed",
        }
    }

    pub fn message(&self) -> &'static str {
        match self {
            ScimEvent::Forbidden => "Forbidden",
            ScimEvent::Conflict => "Conflict",
            _ => "Internal Server Error",
        }
    }

    /// Los eventos SCIM no son métricas en el esquema.
    pub fn otel_metric_name(&self) -> &'static str {
        ""
    }

    pub fn prom_metric_name(&self) -> &'static str {
        ""
    }
}

impl From<ScimEvent> for EventType {
    fn from(event: ScimEvent) -> Self {
        EventType::Scim(event)
    }
}

impl ScimEvent {
    pub fn into_err(self) -> crate::Error {
        EventType::Scim(self).into_err()
    }
}

#[cfg(test)]
mod pruebas {
    use super::*;

    #[test]
    fn eventos_scim_ida_y_vuelta() {
        for event in SCIM_EVENTS {
            let et = EventType::Scim(event);
            assert_eq!(EventType::parse(et.as_str()), Some(et), "{}", et.as_str());
            assert_eq!(EventType::from_id(et.to_id()), Some(et));
            assert!((et.to_id() as usize) < crate::TOTAL_EVENT_COUNT);
            assert!(EventType::variants().contains(&et));
        }
        let mut ids: Vec<u16> = EventType::variants().iter().map(|e| e.to_id()).collect();
        ids.sort_unstable();
        let total = ids.len();
        ids.dedup();
        assert_eq!(total, ids.len(), "ids de evento repetidos");
    }
}
