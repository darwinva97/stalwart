/*
 * SPDX-FileCopyrightText: 2026 bezenti
 *
 * SPDX-License-Identifier: AGPL-3.0-only
 */

//! Posición de replicación de MySQL y MariaDB para las réplicas de lectura (edición bezenti, fase 6).

use super::{MysqlStore, into_error};
use crate::backend::bezenti::replicas::Posicion;
use mysql_async::prelude::Queryable;

impl MysqlStore {
    /// MySQL: `@@GLOBAL.gtid_executed` (solo con `gtid_mode = ON`). MariaDB: `@@GLOBAL.gtid_binlog_pos` en el
    /// primario (solo con binlog) y `@@GLOBAL.gtid_slave_pos` en la réplica. Sin GTID devuelve `None`: la
    /// posición no se puede verificar y las réplicas no se usan.
    pub(crate) async fn posicion_replicacion(
        &self,
        primario: bool,
    ) -> trc::Result<Option<Posicion>> {
        let mut conn = self.conn_pool.get_conn().await.map_err(into_error)?;
        let version: Option<String> = conn
            .query_first("SELECT VERSION()")
            .await
            .map_err(into_error)?;
        let mariadb = version.is_some_and(|v| v.to_ascii_lowercase().contains("mariadb"));

        if mariadb {
            if primario {
                let binlog: Option<i64> = conn
                    .query_first("SELECT @@GLOBAL.log_bin")
                    .await
                    .map_err(into_error)?;
                if binlog.unwrap_or(0) == 0 {
                    return Ok(None);
                }
            }
            let consulta = if primario {
                "SELECT @@GLOBAL.gtid_binlog_pos"
            } else {
                "SELECT @@GLOBAL.gtid_slave_pos"
            };
            let texto: Option<String> = conn.query_first(consulta).await.map_err(into_error)?;
            Ok(texto.and_then(|t| Posicion::gtid_mariadb(&t)))
        } else {
            let modo: Option<String> = conn
                .query_first("SELECT @@GLOBAL.gtid_mode")
                .await
                .map_err(into_error)?;
            if !modo.is_some_and(|m| m.eq_ignore_ascii_case("ON")) {
                return Ok(None);
            }
            let texto: Option<String> = conn
                .query_first("SELECT @@GLOBAL.gtid_executed")
                .await
                .map_err(into_error)?;
            Ok(texto.and_then(|t| Posicion::gtid_mysql(&t.replace(['\n', '\r'], ""))))
        }
    }
}
