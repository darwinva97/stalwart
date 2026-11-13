/*
 * SPDX-FileCopyrightText: 2026 bezenti
 *
 * SPDX-License-Identifier: AGPL-3.0-only
 */

//! Informes externos por inquilino (edición bezenti, fase 8; `bezenti/especificaciones/multi-inquilino.md`, §4).
//!
//! Un informe DMARC, TLS o ARF recibido se etiqueta con el inquilino del dominio al que se refiere, para que su
//! administrador lo vea (y solo él y el operador). Reglas:
//!
//! - Se miran primero los dominios del contenido del informe (el dominio publicado en DMARC, las políticas de TLS,
//!   los dominios denunciados en ARF) que son dominios de este servidor.
//! - Si ninguno lo es, los dominios de los destinatarios del informe.
//! - Si todos los dominios de este servidor encontrados son del mismo inquilino, el informe es de ese inquilino.
//!   Si hay dominios de varios inquilinos, o alguno es del operador (sin inquilino), el informe queda solo para el
//!   operador: así nunca se enseña a un inquilino un informe que también habla de otro.

use common::Server;
use trc::AddContext;
use types::id::Id;

/// Inquilino de un informe a partir de los dominios de su contenido y de sus destinatarios.
pub(crate) async fn report_tenant(
    server: &Server,
    report_domains: Vec<String>,
    recipients: &[String],
) -> Option<Id> {
    match tenant_of_domains(server, report_domains.iter().map(String::as_str)).await {
        Some(result) => result,
        None => tenant_of_domains(
            server,
            recipients
                .iter()
                .filter_map(|address| address.rsplit_once('@').map(|(_, domain)| domain)),
        )
        .await
        .flatten(),
    }
    .map(Id::from)
}

/// `None`: ninguno de los dominios es de este servidor. `Some(None)`: hay dominios de este servidor pero no de un
/// único inquilino. `Some(Some(t))`: todos los de este servidor son del inquilino `t`.
async fn tenant_of_domains<'x>(
    server: &Server,
    domains: impl Iterator<Item = &'x str>,
) -> Option<Option<u32>> {
    let mut found: Option<Option<u32>> = None;
    for domain in domains {
        let domain = domain.trim().trim_end_matches('.').to_lowercase();
        if domain.is_empty() {
            continue;
        }
        let tenant = match server.domain(&domain).await.caused_by(trc::location!()) {
            Ok(Some(domain)) => domain.tenant_id,
            Ok(None) => continue,
            Err(err) => {
                trc::error!(err.details("No se pudo resolver el dominio de un informe"));
                return Some(None);
            }
        };
        match (found, tenant) {
            (None, tenant) => found = Some(tenant),
            (Some(Some(previous)), Some(tenant)) if previous == tenant => {}
            _ => return Some(None),
        }
    }
    found
}
