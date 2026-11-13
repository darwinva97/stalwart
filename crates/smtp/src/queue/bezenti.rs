/*
 * SPDX-FileCopyrightText: 2026 bezenti
 *
 * SPDX-License-Identifier: AGPL-3.0-only
 */

//! Cola SMTP por inquilino (edición bezenti, fase 8).
//!
//! Especificación: `bezenti/especificaciones/multi-inquilino.md`, §5 («Cola y límites de envío por
//! inquilino»). Resumen de lo que hay aquí:
//!
//! - **De quién es un mensaje** ([`Metadata::Tenant`]): lo pone quien encola y solo cuando lo sabe de verdad: la
//!   sesión SMTP autenticada o el envío JMAP (inquilino de la cuenta), los mensajes que genera la entrega local
//!   para una cuenta (redirecciones y respuestas de Sieve) y los avisos de entrega (DSN) de un mensaje que ya
//!   era de un inquilino. El correo entrante sin autenticar no es de nadie (es del operador), aunque su
//!   remitente diga ser de un dominio de un inquilino: así nadie puede colar mensajes en la vista de otro.
//! - **Límites** ([`admit`]): mensajes y destinatarios por intervalo (`bezentiMessageRate`,
//!   `bezentiRecipientRate`) y mensajes a la vez en la cola (`bezentiMaxQueuedMessages`) de cada inquilino y de
//!   sus ancestros (lo de un hijo cuenta para su revendedor). Los ritmos solo frenan los envíos autenticados; el
//!   contador de la cola cuenta todo lo del inquilino pero solo rechaza los envíos autenticados (un aviso de
//!   entrega nunca se pierde por esto).
//! - **Reparto justo**: el valor de la clave de cada evento de la cola lleva el inquilino ([`event_value`]); el
//!   gestor de la cola lo lee al buscar mensajes listos y reparte los hilos de entrega entre inquilinos
//!   (`queue/manager.rs`).

use super::{
    ArchivedMessage, ArchivedMetadata, FROM_AUTHENTICATED, Message, MessageWrapper, Metadata,
    quota::QuotaReservation,
};
use common::{KV_RATE_LIMIT_SMTP, Server};
use registry::schema::structs::Rate;
use store::dispatch::lookup::{KeyValue, RateWindow};
use store::write::now;
use trc::AddContext;

/// Grupo de reparto de los mensajes que no son de ningún inquilino (los del operador y el correo entrante).
pub const NO_TENANT: u32 = u32::MAX;

/// Cuántos eventos de más (además de los que busca el código libre) se leen en cada búsqueda de mensajes listos
/// de una cola, para encontrar los de otros inquilinos detrás de un envío masivo. Se puede cambiar con la variable
/// de entorno `BEZENTI_COLA_VENTANA`.
pub fn scan_window() -> usize {
    static WINDOW: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *WINDOW.get_or_init(|| {
        std::env::var("BEZENTI_COLA_VENTANA")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(2048)
    })
}

/// Por qué no se acepta un mensaje en la cola.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rejection {
    /// Se ha pasado el ritmo de mensajes del inquilino `0`.
    Messages(u32),
    /// Se ha pasado el ritmo de destinatarios del inquilino `0`.
    Recipients(u32),
}

impl Message {
    /// Inquilino del mensaje, si es de alguno.
    pub fn tenant_id(&self) -> Option<u32> {
        self.metadata.iter().find_map(|metadata| match metadata {
            Metadata::Tenant { tenant_id } => Some(*tenant_id),
            _ => None,
        })
    }
}

impl ArchivedMessage {
    /// Inquilino del mensaje, si es de alguno.
    pub fn tenant_id(&self) -> Option<u32> {
        self.metadata.iter().find_map(|metadata| match metadata {
            ArchivedMetadata::Tenant { tenant_id } => Some(tenant_id.to_native()),
            _ => None,
        })
    }
}

/// Valor que se guarda en la clave de un evento de la cola: el inquilino del mensaje (4 bytes) o nada.
pub fn event_value(message: &Message) -> Vec<u8> {
    message
        .tenant_id()
        .map(|tenant_id| tenant_id.to_be_bytes().to_vec())
        .unwrap_or_default()
}

/// Grupo de reparto de un evento a partir de su valor ([`NO_TENANT`] si no lleva inquilino).
pub fn event_group(value: &[u8]) -> u32 {
    value
        .get(..4)
        .and_then(|bytes| bytes.try_into().ok())
        .map(u32::from_be_bytes)
        .unwrap_or(NO_TENANT)
}

/// Metadato de inquilino para un mensaje que se va a encolar.
pub fn tenant_metadata(tenant_id: Option<u32>) -> Option<Metadata> {
    tenant_id.map(|tenant_id| Metadata::Tenant { tenant_id })
}

/// Inquilino de la cuenta dueña de una dirección local (para los mensajes que genera la entrega local).
pub async fn tenant_of_address(server: &Server, address: &str) -> Option<u32> {
    let domain = address.rsplit_once('@')?.1;
    match server.domain(domain).await {
        Ok(domain) => domain.and_then(|domain| domain.tenant_id),
        Err(err) => {
            trc::error!(err.caused_by(trc::location!()));
            None
        }
    }
}

/// Clave del contador de mensajes en la cola de un inquilino (contadores de cuota de la cola, `QuotaCount`).
pub fn queue_counter_key(tenant_id: u32) -> u128 {
    let mut hasher = store::blake3::Hasher::new();
    hasher.update(b"bezenti:cola-inquilino:");
    hasher.update(&tenant_id.to_be_bytes());
    let hash = hasher.finalize();
    u128::from_be_bytes(hash.as_bytes()[..16].try_into().unwrap())
}

/// Admisión de un mensaje en la cola: deja el inquilino en `metadata` (si solo se conocía por la cuenta del envío
/// JMAP), comprueba los ritmos del inquilino y de sus ancestros y devuelve las reservas del contador de mensajes
/// en cola (que se escriben con el mensaje y se liberan al salir de la cola).
pub async fn admit(
    server: &Server,
    message: &MessageWrapper,
    metadata: &mut Vec<Metadata>,
) -> Result<Vec<QuotaReservation>, Rejection> {
    let mut tenant_id = metadata.iter().find_map(|metadata| match metadata {
        Metadata::Tenant { tenant_id } => Some(*tenant_id),
        _ => None,
    });
    if tenant_id.is_none()
        && let Some(account_id) = metadata.iter().find_map(|metadata| match metadata {
            Metadata::Submission { account_id } => Some(*account_id),
            _ => None,
        })
    {
        tenant_id = match server.account(account_id).await {
            Ok(account) => account.tenant_id,
            Err(err) => {
                trc::error!(err.span_id(message.span_id).caused_by(trc::location!()));
                None
            }
        };
        metadata.extend(tenant_metadata(tenant_id));
    }
    let Some(tenant_id) = tenant_id else {
        return Ok(Vec::new());
    };
    let chain = match server.tenant_ancestors(tenant_id).await {
        Ok(chain) => chain,
        Err(err) => {
            // Sin poder leer el inquilino no se frena el correo: se encola sin límites.
            trc::error!(
                err.span_id(message.span_id)
                    .details("No se pudieron leer los límites del inquilino")
                    .caused_by(trc::location!())
            );
            return Ok(Vec::new());
        }
    };
    let is_submission = message.message.flags & FROM_AUTHENTICATED != 0;

    // Ritmos (solo los envíos autenticados).
    if is_submission {
        let recipients = message.message.recipients.len().max(1) as i64;
        let mut taken: Vec<(Vec<u8>, i64)> = Vec::new();
        let mut rejection = None;
        'chain: for tenant in &chain {
            for (rate, amount, kind) in [
                (&tenant.message_rate, 1, b'm'),
                (&tenant.recipient_rate, recipients, b'r'),
            ] {
                let Some(rate) = rate else { continue };
                match take_rate(server, tenant.id, kind, rate, amount).await {
                    Ok(Some(bucket)) => taken.push((bucket, amount)),
                    Ok(None) => {
                        rejection = Some(if kind == b'm' {
                            Rejection::Messages(tenant.id)
                        } else {
                            Rejection::Recipients(tenant.id)
                        });
                        break 'chain;
                    }
                    Err(err) => {
                        trc::error!(err.span_id(message.span_id).caused_by(trc::location!()));
                    }
                }
            }
        }
        if let Some(rejection) = rejection {
            // Lo ya apuntado en los otros niveles se devuelve: el mensaje no sale.
            for (bucket, amount) in taken {
                let _ = server
                    .in_memory_store()
                    .key_incr(KeyValue::new(bucket, -amount).expires(60))
                    .await;
            }
            let (tenant, what) = match rejection {
                Rejection::Messages(tenant) => (tenant, "messages"),
                Rejection::Recipients(tenant) => (tenant, "recipients"),
            };
            trc::event!(
                Queue(trc::QueueEvent::RateLimitExceeded),
                SpanId = message.span_id,
                Name = format!("tenant {tenant} {what}"),
                AccountId = tenant,
            );
            return Err(rejection);
        }
    }

    // Contador de mensajes en cola: cuenta siempre (para que esté al día si se pone un límite más tarde) y solo
    // limita los envíos autenticados.
    Ok(chain
        .iter()
        .map(|tenant| QuotaReservation::Count {
            key: queue_counter_key(tenant.id),
            id: 0,
            max: tenant
                .max_queued_messages
                .filter(|_| is_submission)
                .unwrap_or(u64::MAX),
        })
        .collect())
}

/// Suma `amount` al contador del intervalo actual; `Ok(None)` si se pasa del ritmo (y deshace la suma).
async fn take_rate(
    server: &Server,
    tenant_id: u32,
    kind: u8,
    rate: &Rate,
    amount: i64,
) -> trc::Result<Option<Vec<u8>>> {
    let window = RateWindow::new(rate, now());
    let mut key = Vec::with_capacity(16);
    key.extend_from_slice(b"bzt:");
    key.push(kind);
    key.extend_from_slice(&tenant_id.to_be_bytes());
    let bucket = window.bucket(KV_RATE_LIMIT_SMTP, &key);
    let store = server.in_memory_store();
    let total = store
        .key_incr(KeyValue::new(bucket.clone(), amount).expires(window.expires_in))
        .await
        .caused_by(trc::location!())?;
    if total <= rate.count as i64 {
        Ok(Some(bucket))
    } else {
        store
            .key_incr(KeyValue::new(bucket, -amount).expires(window.expires_in))
            .await
            .caused_by(trc::location!())?;
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grupo_de_evento() {
        assert_eq!(event_group(&[]), NO_TENANT);
        assert_eq!(event_group(&7u32.to_be_bytes()), 7);
        assert_ne!(queue_counter_key(1), queue_counter_key(2));
    }
}
