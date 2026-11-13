/*
 * SPDX-FileCopyrightText: 2026 bezenti
 *
 * SPDX-License-Identifier: AGPL-3.0-only
 */

//! Piezas escritas a mano que necesita el modelo de datos de pago generado por `bezenti/generador`.
//!
//! El modelo en sí está en `schema/` (generado); aquí van sus pruebas y, desde la fase 2, los accesores de
//! `memberTenantId` (`inquilino_gen.rs`, generado por `bezenti/generador/gen_inquilino.py`).
//!
//! Las claves de la IA del enum `Key` de las trazas (`x:TraceKeyValue.key`, que es `trc::Key`: `model`,
//! `inputTokens`, `outputTokens`, `cachedTokens`, `finishReason`, `category`, `probability`, `confidence`,
//! `tier`) están en `trc` desde la fase 4 (feature `bezenti` de `trc`, ver `bezenti/especificaciones/ia.md`).

mod inquilino_gen;

#[cfg(test)]
mod pruebas;
