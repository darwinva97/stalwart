/*
 * SPDX-FileCopyrightText: 2026 bezenti
 *
 * SPDX-License-Identifier: AGPL-3.0-only
 */

//! Funciones de la edición bezenti que viven en `common` (ver `bezenti/README.md`).
//!
//! - `inquilino`: multi-inquilino y revendedores (fase 2): caché de inquilinos, ámbito de un administrador,
//!   techo de permisos y cuotas. Especificación: `bezenti/especificaciones/multi-inquilino.md`.
//! - `logo`: el logo de marca por dominio o inquilino (`GET /logo`).
//! - `enmascaradas`: direcciones enmascaradas (fase 3): formato, validación y resolución en la recepción.
//!   Especificación: `bezenti/especificaciones/enmascaradas.md`.
//! - `archivo`: archivo de elementos borrados (fase 3): formato y claves del archivo, pendientes de archivar.
//!   Especificación: `bezenti/especificaciones/archivo.md`.
//! - `ia`: modelos de IA, clasificador antispam con IA y `llm_prompt` en Sieve (fase 4). Especificación:
//!   `bezenti/especificaciones/ia.md`.
//! - `telemetria`: historial de entregas y de métricas, alertas y telemetría en vivo (fase 5).
//!   Especificación: `bezenti/especificaciones/telemetria.md`.

pub mod archivo;
pub(crate) mod arranque;
pub mod enmascaradas;
pub mod ia;
pub mod inquilino;
pub mod logo;
pub mod telemetria;
