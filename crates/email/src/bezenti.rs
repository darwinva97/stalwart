/*
 * SPDX-FileCopyrightText: 2026 bezenti
 *
 * SPDX-License-Identifier: AGPL-3.0-only
 */

//! Edición bezenti, fase 3: archivo de los mensajes borrados (ver `bezenti/especificaciones/archivo.md`).
//!
//! Al borrar definitivamente un mensaje (JMAP, IMAP `EXPUNGE`, POP3 `DELE`, borrado de un buzón con sus
//! mensajes o vaciado automático de la Papelera y el Spam) se apunta un «pendiente de archivar» con lo que el
//! borrado hace perder: los buzones y las etiquetas. El elemento del archivo lo crea después la cola del índice,
//! que es la que suelta el blob del mensaje (`services::index_queue`). La destrucción de una cuenta no pasa por
//! aquí: sus datos no se archivan.

use crate::message::messagedata::MessageData;
use common::{
    Server,
    bezenti::archivo::{EmailPendiente, clave_pendiente},
};
use store::write::{BatchBuilder, now};
use types::keyword::Keyword;

/// Apunta en `batch` que el mensaje `document_id` se debe archivar (si el archivo está activado).
pub fn mark_email_for_archive(
    server: &Server,
    batch: &mut BatchBuilder,
    account_id: u32,
    document_id: u32,
    data: &MessageData,
) {
    let Some(retention) = server.archive_retention() else {
        return;
    };
    let pendiente = EmailPendiente {
        hasta: now() + retention,
        buzones: data.mailboxes.iter().map(|m| m.mailbox_id).collect(),
        // `$deleted` no se guarda: al restaurar, el mensaje no debe volver marcado para borrar. Los indicadores de
        // adjuntos los pone el servidor al ingresarlo.
        etiquetas: data
            .keywords()
            .filter(|keyword| {
                !matches!(
                    keyword,
                    Keyword::Deleted | Keyword::HasAttachment | Keyword::HasNoAttachment
                )
            })
            .map(|keyword| keyword.to_string())
            .collect(),
        recibido: data.received_at,
    };
    batch.set(
        clave_pendiente(account_id, document_id),
        pendiente.serializar(),
    );
}
