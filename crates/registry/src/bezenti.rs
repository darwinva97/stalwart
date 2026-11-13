/*
 * SPDX-FileCopyrightText: 2026 bezenti
 *
 * SPDX-License-Identifier: AGPL-3.0-only
 */

//! Piezas escritas a mano que necesita el modelo de datos de pago generado por `bezenti/generador`.
//!
//! El modelo en sí está en `schema/` (generado); aquí van sus pruebas.
//!
//! TODO (fases 4 y 5): el esquema completo añade al enum `Key` de las trazas (`x:TraceKeyValue.key`, que
//! es `trc::Key`) claves de la IA (`model`, `inputTokens`, `outputTokens`, `cachedTokens`, `finishReason`,
//! `category`, `probability`, `confidence`, `tier`) que `trc::Key` todavía no tiene; se añadirán en `trc`
//! cuando se implementen la IA y la telemetría.

#[cfg(test)]
mod pruebas;
