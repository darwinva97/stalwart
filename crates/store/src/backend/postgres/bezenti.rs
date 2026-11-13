/*
 * SPDX-FileCopyrightText: 2026 bezenti
 *
 * SPDX-License-Identifier: AGPL-3.0-only
 */

//! Posición de replicación de PostgreSQL para las réplicas de lectura (edición bezenti, fase 6).

use super::{PostgresStore, into_error, into_pool_error};
use crate::backend::bezenti::replicas::Posicion;

impl PostgresStore {
    /// En el primario, `pg_current_wal_lsn()` (lo escrito); en una réplica física, `pg_last_wal_replay_lsn()`
    /// (lo aplicado, que es lo que ven las consultas). Un servidor que no está en recuperación no es una
    /// réplica física: como réplica devuelve `None` (no se puede comparar su posición).
    pub(crate) async fn posicion_replicacion(
        &self,
        primario: bool,
    ) -> trc::Result<Option<Posicion>> {
        let conn = self.conn_pool.get().await.map_err(into_pool_error)?;
        let fila = conn
            .query_one(
                concat!(
                    "SELECT pg_is_in_recovery(), ",
                    "(pg_current_wal_lsn() - '0/0'::pg_lsn)::bigint ",
                    "FROM (SELECT 1) AS x WHERE NOT pg_is_in_recovery() ",
                    "UNION ALL ",
                    "SELECT pg_is_in_recovery(), ",
                    "(pg_last_wal_replay_lsn() - '0/0'::pg_lsn)::bigint ",
                    "FROM (SELECT 1) AS x WHERE pg_is_in_recovery()"
                ),
                &[],
            )
            .await
            .map_err(into_error)?;
        let en_recuperacion: bool = fila.try_get(0).map_err(into_error)?;
        let lsn: Option<i64> = fila.try_get(1).map_err(into_error)?;
        Ok(match (primario, en_recuperacion, lsn) {
            (true, false, Some(lsn)) | (false, true, Some(lsn)) => Some(Posicion::Lsn(lsn as u64)),
            _ => None,
        })
    }
}
