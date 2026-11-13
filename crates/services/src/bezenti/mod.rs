/*
 * SPDX-FileCopyrightText: 2026 bezenti
 *
 * SPDX-License-Identifier: AGPL-3.0-only
 */

//! Funciones de la edición bezenti que viven en `services` (ver `bezenti/README.md`).
//!
//! - `archivo`: archivo de borrados (fase 3): crear los elementos de los mensajes borrados y la tarea
//!   `RestoreArchivedItem`. Especificación: `bezenti/especificaciones/archivo.md`.
//! - `telemetria`: historial de métricas y alertas (fase 5). Especificación:
//!   `bezenti/especificaciones/telemetria.md`.

pub(crate) mod archivo;
pub mod telemetria;
