/*
 * SPDX-FileCopyrightText: 2026 bezenti
 *
 * SPDX-License-Identifier: AGPL-3.0-only
 */

//! Archivo de borrados, lado del servidor (fase 3; `bezenti/especificaciones/archivo.md`):
//!
//! - [`archive_deleted_email`]: la cola del índice, al soltar el blob de un mensaje borrado que estaba apuntado
//!   para el archivo, crea su elemento `x:ArchivedItem` con el remitente, el asunto, el tamaño y la fecha de
//!   recepción (y los buzones y etiquetas que se apuntaron al borrarlo).
//! - [`RestoreArchivedItemTask`]: la tarea `RestoreArchivedItem` devuelve el elemento a la cuenta y lo quita del
//!   archivo.

use crate::task_manager::outcome::Outcome;
use common::{
    Server,
    auth::BuildAccessToken,
    bezenti::archivo::{ElementoArchivado, EmailPendiente},
};
use email::{
    cache::{MessageCacheFetch, mailbox::MailboxCacheAccess},
    mailbox::INBOX_ID,
    message::{
        ingest::{EmailIngest, IngestEmail, IngestSource},
        metadata::{AddressHeader, MetadataRow, Occurrence},
    },
};
use mail_parser::MessageParser;
use registry::{
    schema::{enums::ArchivedItemType, structs::TaskRestoreArchivedItem},
    types::EnumImpl,
};
use store::write::{BatchBuilder, now};
use trc::AddContext;
use types::keyword::Keyword;

/// Crea en `batch` el elemento archivado de un mensaje cuyo blob suelta la cola del índice, si al borrarlo se
/// apuntó para el archivo. Borra el «pendiente» en cualquier caso.
pub(crate) async fn archive_deleted_email(
    server: &Server,
    batch: &mut BatchBuilder,
    account_id: u32,
    document_id: u32,
    row: Option<&MetadataRow>,
) -> trc::Result<()> {
    let Some(pendiente) = server
        .take_pending_email_archive(batch, account_id, document_id)
        .await?
    else {
        return Ok(());
    };
    let Some(row) = row else {
        return Ok(());
    };
    let metadata = row.unarchive().caused_by(trc::location!())?;
    let envelope = metadata.root().envelope();
    let from = envelope
        .addresses(AddressHeader::From, Occurrence::All)
        .first()
        .map(|mailbox| match (mailbox.name, mailbox.address) {
            (Some(name), Some(address)) if !name.is_empty() => format!("{name} <{address}>"),
            (_, Some(address)) => address.to_string(),
            (Some(name), None) => name.to_string(),
            (None, None) => String::new(),
        })
        .unwrap_or_default();
    let EmailPendiente {
        hasta,
        buzones,
        etiquetas,
        recibido,
    } = pendiente;

    let mut item = ElementoArchivado {
        tipo: ArchivedItemType::Email.as_str().to_string(),
        account_id,
        tamano: metadata.blob_size() as u64,
        archivado: now(),
        hasta,
        creado: recibido,
        nombre: envelope.subject().unwrap_or_default().to_string(),
        de: from,
        contenedores: buzones,
        etiquetas,
        ..Default::default()
    }
    .con_hash(&metadata.blob_hash());
    // Las cabeceras que añadió el servidor al recibirlo no están en el blob: se guardan con el elemento.
    let cuerpo = metadata.blob_body_offset();
    if metadata.headers_len() != cuerpo {
        let cabeceras = row.raw_headers().caused_by(trc::location!())?;
        item = item.con_cabeceras(&cabeceras, cuerpo);
    }
    server.archive_item_create(batch, item).await?;
    Ok(())
}

pub(crate) trait RestoreArchivedItemTask: Sync + Send {
    fn restore_archived_item(
        &self,
        task: &TaskRestoreArchivedItem,
    ) -> impl Future<Output = Outcome> + Send;
}

impl RestoreArchivedItemTask for Server {
    async fn restore_archived_item(&self, task: &TaskRestoreArchivedItem) -> Outcome {
        match restore(self, task).await {
            Ok(outcome) => outcome,
            Err(err) => {
                let result = Outcome::error(&err);
                trc::error!(
                    err.account_id(task.account_id.document_id())
                        .details("No se pudo restaurar un elemento archivado")
                );
                result
            }
        }
    }
}

async fn restore(server: &Server, task: &TaskRestoreArchivedItem) -> trc::Result<Outcome> {
    let account_id = task.account_id.document_id();
    let Some((item_id, item)) = server
        .archived_item_by_link(
            account_id,
            &task.blob_id.hash,
            task.archived_until.timestamp() as u64,
        )
        .await?
    else {
        return Ok(Outcome::Obsolete(
            "El elemento ya no está en el archivo (restaurado, borrado o purgado)",
        ));
    };
    if server.try_account(account_id).await?.is_none() {
        return Ok(Outcome::Obsolete("La cuenta del elemento ya no existe"));
    }

    let result = match item.tipo() {
        ArchivedItemType::Email => restore_email(server, account_id, &item).await?,
        // Fase 8: archivos, contactos, eventos y guiones Sieve.
        tipo => restore_other(server, account_id, tipo, &item).await?,
    };

    let mut batch = BatchBuilder::new();
    match result {
        Ok(()) => {
            // Fuera del archivo: el blob ya está enlazado al objeto restaurado.
            item.borrar(&mut batch, item_id);
            server.store().write_batch(&mut batch).await?;
            Ok(Outcome::done())
        }
        Err(reason) => {
            // Vuelve a «archived» para que se pueda pedir otra vez cuando se resuelva el problema.
            let mut item = item;
            item.restaurar = false;
            batch.set(
                common::bezenti::archivo::clave_item(item_id),
                item.serializar(),
            );
            server.store().write_batch(&mut batch).await?;
            Ok(Outcome::fatal(reason))
        }
    }
}

/// Fase 8: restaura un archivo, contacto, evento o guion Sieve (`bezenti/especificaciones/archivo.md`, §4).
async fn restore_other(
    server: &Server,
    account_id: u32,
    tipo: ArchivedItemType,
    item: &ElementoArchivado,
) -> trc::Result<Result<(), String>> {
    let hash = item.blob_hash();
    if tipo == ArchivedItemType::FileNode {
        // El archivo vuelve con el mismo blob: basta con que siga en el almacén.
        if !server
            .store()
            .blob_exists(&hash)
            .await
            .caused_by(trc::location!())?
        {
            return Ok(Err("El contenido archivado ya no existe".to_string()));
        }
        return groupware::bezenti::restore_file(server, account_id, item, hash).await;
    }
    let Some(bytes) = server
        .get_blob(hash.as_slice(), 0..usize::MAX)
        .await
        .caused_by(trc::location!())?
    else {
        return Ok(Err("El contenido archivado ya no existe".to_string()));
    };
    match tipo {
        ArchivedItemType::ContactCard => {
            groupware::bezenti::restore_contact(server, account_id, item, &bytes).await
        }
        ArchivedItemType::CalendarEvent => {
            groupware::bezenti::restore_event(server, account_id, item, &bytes).await
        }
        ArchivedItemType::SieveScript => {
            email::bezenti::restore_sieve_script(server, account_id, &item.nombre, hash, &bytes)
                .await
        }
        ArchivedItemType::Email | ArchivedItemType::FileNode => unreachable!(),
    }
}

async fn restore_email(
    server: &Server,
    account_id: u32,
    item: &ElementoArchivado,
) -> trc::Result<Result<(), String>> {
    let hash = item.blob_hash();
    let Some(bytes) = server
        .get_blob(hash.as_slice(), 0..usize::MAX)
        .await
        .caused_by(trc::location!())?
    else {
        return Ok(Err("El contenido archivado ya no existe".to_string()));
    };
    let bytes = item.mensaje_integro(bytes);

    // A los buzones originales que sigan existiendo; si no queda ninguno, a la bandeja de entrada.
    let cache = server
        .get_cached_messages(account_id)
        .await
        .caused_by(trc::location!())?;
    let mut mailbox_ids = item
        .contenedores
        .iter()
        .copied()
        .filter(|id| cache.has_mailbox_id(id))
        .collect::<Vec<_>>();
    if mailbox_ids.is_empty() {
        mailbox_ids.push(INBOX_ID);
    }
    let keywords = item
        .etiquetas
        .iter()
        .map(|k| Keyword::parse(k))
        .collect::<Vec<_>>();
    let access_token = server
        .access_token(account_id)
        .await
        .caused_by(trc::location!())?
        .build();

    match server
        .email_ingest(IngestEmail {
            raw_message: &bytes,
            message: MessageParser::new().parse(&bytes).as_ref(),
            // Si el mensaje llevaba cabeceras del servidor, el contenido íntegro es otro blob.
            blob_hash: item.cabeceras.is_none().then_some(&hash),
            access_token: &access_token,
            mailbox_ids,
            keywords,
            received_at: Some(item.creado),
            source: IngestSource::Restore,
            metadata: None,
            session_id: 0,
        })
        .await
    {
        Ok(_) => Ok(Ok(())),
        Err(mut err)
            if err.matches(trc::EventType::MessageIngest(
                trc::MessageIngestEvent::Error,
            )) || err.matches(trc::EventType::Limit(trc::LimitEvent::Quota)) =>
        {
            Ok(Err(err
                .take_value(trc::Key::Reason)
                .and_then(|v| v.into_string())
                .map_or_else(
                    || "No se pudo restaurar el mensaje (cuota o mensaje no válido)".to_string(),
                    Into::into,
                )))
        }
        Err(err) => Err(err.caused_by(trc::location!())),
    }
}
