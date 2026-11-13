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

/// Fase 8: archiva (en `batch`) un guion Sieve que borra un usuario (JMAP o ManageSieve). Su contenido es su blob.
pub async fn archive_sieve_script(
    server: &Server,
    batch: &mut BatchBuilder,
    account_id: u32,
    script: &crate::sieve::ArchivedSieveScript,
) -> trc::Result<()> {
    use common::bezenti::archivo::{ContenidoArchivado, ElementoArchivado, archivar_borrados};
    use registry::{schema::enums::ArchivedItemType, types::EnumImpl};
    if !archivar_borrados() || server.archive_retention().is_none() {
        return Ok(());
    }
    let item = ElementoArchivado {
        tipo: ArchivedItemType::SieveScript.as_str().to_string(),
        account_id,
        creado: now(),
        nombre: script.name.to_string(),
        ..Default::default()
    };
    server
        .archive_deleted_item(
            batch,
            item,
            ContenidoArchivado::Blob {
                hash: types::blob_hash::BlobHash::from(&script.blob_hash),
                tamano: script.size.to_native() as u64,
            },
        )
        .await
}

/// Fase 8: restaura un guion Sieve archivado (inactivo, con su nombre si está libre). `Err` con el motivo si no se
/// puede (cuota, guion que ya no compila).
pub async fn restore_sieve_script(
    server: &Server,
    account_id: u32,
    name: &str,
    hash: types::blob_hash::BlobHash,
    contents: &[u8],
) -> trc::Result<Result<(), String>> {
    use crate::sieve::SieveScript;
    use common::storage::index::ObjectIndexBuilder;
    use registry::schema::enums::StorageQuota;
    use trc::AddContext;
    use types::{collection::Collection, field::SieveField};

    let account = server.account(account_id).await?;
    let scripts = server
        .document_ids(account_id, Collection::SieveScript, SieveField::Name)
        .await
        .caused_by(trc::location!())?
        .len();
    if scripts >= server.object_quota(account.object_quotas(), StorageQuota::MaxSieveScripts) as u64
    {
        return Ok(Err(
            "La cuenta ya tiene el máximo de guiones Sieve".to_string()
        ));
    }
    let compiled = match server.core.sieve.untrusted_compiler.compile(contents) {
        Ok(compiled) => compiled,
        Err(err) => return Ok(Err(format!("El guion ya no compila: {err}"))),
    };
    // Nombre libre: el original o `<nombre>-<n>`.
    let mut nombre = name.to_string();
    for n in 1..1000 {
        if server
            .document_ids_matching(
                account_id,
                Collection::SieveScript,
                SieveField::Name,
                nombre.to_lowercase().as_bytes(),
            )
            .await
            .caused_by(trc::location!())?
            .is_empty()
        {
            break;
        }
        nombre = format!("{name}-{n}");
    }
    let mut batch = BatchBuilder::new();
    let slot = batch.reserve_document_id(account_id, Collection::SieveScript);
    batch.with_account_id(account_id);
    server.limit_disk_quota(&mut batch, &account);
    batch
        .with_collection(Collection::SieveScript)
        .create_document(slot)
        .custom(
            ObjectIndexBuilder::<(), _>::new()
                .with_changes(
                    SieveScript::new(nombre, hash)
                        .with_size(contents.len() as u32)
                        .with_script(&compiled),
                )
                .with_changed_by(account.changed_by()),
        )
        .caused_by(trc::location!())?;
    match server.commit_batch(batch).await {
        Ok(_) => Ok(Ok(())),
        Err(err) if err.matches(trc::EventType::Limit(trc::LimitEvent::Quota)) => Ok(Err(
            "No hay cuota para restaurarlo (de la cuenta o del inquilino)".to_string(),
        )),
        Err(err) => Err(err.caused_by(trc::location!())),
    }
}
