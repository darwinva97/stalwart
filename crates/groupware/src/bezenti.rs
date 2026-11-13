/*
 * SPDX-FileCopyrightText: 2026 bezenti
 *
 * SPDX-License-Identifier: AGPL-3.0-only
 */

//! Archivo de archivos (WebDAV), eventos de calendario y contactos borrados. Edición bezenti, fase 8.
//!
//! Especificación: `bezenti/especificaciones/archivo.md`, §3 y §4. Lo que hay aquí:
//!
//! - **Archivar** ([`archive_file`], [`archive_contact`], [`archive_event`]): lo llaman las funciones de borrado de
//!   este crate cuando un elemento desaparece de verdad (no cuando solo sale de uno de sus contenedores) y solo si
//!   el borrado lo pidió un usuario ([`common::bezenti::archivo::con_archivo`]): los movimientos entre cuentas o
//!   colecciones usan las mismas funciones y no deben archivar nada. Un archivo enlaza su blob; un evento o un
//!   contacto se guarda como blob nuevo con su iCalendar o su vCard completos.
//! - **Restaurar** ([`restore_file`], [`restore_contact`], [`restore_event`]): devuelve el elemento a su
//!   carpeta, calendarios o libretas originales que sigan existiendo (si no, a la raíz o al calendario o libreta
//!   por defecto), con su nombre si está libre, comprobando las cuotas como cualquier alta. Es un elemento nuevo
//!   para los clientes (id nuevo).

use crate::{
    cache::GroupwareCache,
    calendar::{
        ArchivedCalendarEvent, CalendarEvent, CalendarEventContent, CalendarEventData,
        alerts::DefaultAlertsResolver,
        schedule::{EventAlarmScheduler, EventAlarmUsers},
    },
    contact::{ArchivedContactCard, ContactCard, ContactCardContent},
    file::{ArchivedFileNode, ArchivedFileNodeContent, FileNode, FileNodeContent, FileProperties},
};
use calcard::{common::timezone::Tz, icalendar::ICalendar, vcard::VCard};
use common::{
    DavName, GroupwareResources, Server,
    bezenti::archivo::{ContenidoArchivado, ElementoArchivado, archivar_borrados, propiedad_texto},
};
use registry::{
    schema::enums::{ArchivedItemType, StorageQuota},
    types::EnumImpl,
};
use std::sync::Arc;
use store::{
    ValueKey,
    write::{Archive, ArchiveBytes, BatchBuilder, now},
};
use trc::AddContext;
use types::{
    blob_hash::BlobHash,
    collection::{Collection, SyncCollection},
    field::{CalendarEventField, ContactField},
};

/// Etiqueta con el tipo MIME de un archivo archivado.
const MIME: &str = "mime:";
/// Etiqueta de un archivo archivado que era ejecutable.
const EXEC: &str = "exec";

fn activo(server: &Server) -> bool {
    archivar_borrados() && server.archive_retention().is_some()
}

/// Archiva un archivo (no las carpetas) que se borra.
pub async fn archive_file(
    server: &Server,
    batch: &mut BatchBuilder,
    account_id: u32,
    node: &ArchivedFileNode,
) -> trc::Result<()> {
    if !activo(server) {
        return Ok(());
    }
    let ArchivedFileNodeContent::File(file) = &node.content else {
        return Ok(());
    };
    let mut etiquetas = Vec::new();
    if let Some(media_type) = file.media_type.as_ref() {
        etiquetas.push(format!("{MIME}{media_type}"));
    }
    if file.executable {
        etiquetas.push(EXEC.to_string());
    }
    let item = ElementoArchivado {
        tipo: ArchivedItemType::FileNode.as_str().to_string(),
        account_id,
        creado: node.created.to_native().max(0) as u64,
        nombre: node.name.to_string(),
        contenedores: vec![node.parent_id.to_native()],
        etiquetas,
        ..Default::default()
    };
    server
        .archive_deleted_item(
            batch,
            item,
            ContenidoArchivado::Blob {
                hash: BlobHash::from(&file.blob_hash),
                tamano: file.size.to_native() as u64,
            },
        )
        .await
}

/// Archiva un contacto que se borra (con su vCard completa).
pub async fn archive_contact(
    server: &Server,
    batch: &mut BatchBuilder,
    account_id: u32,
    document_id: u32,
    card: &ArchivedContactCard,
    content: Option<&Archive<ArchiveBytes>>,
) -> trc::Result<()> {
    if !activo(server) {
        return Ok(());
    }
    let leido;
    let content = match content {
        Some(content) => content,
        None => match server
            .store()
            .get_value::<Archive<ArchiveBytes>>(ValueKey::property(
                account_id,
                Collection::ContactCard,
                document_id,
                ContactField::Content,
            ))
            .await
            .caused_by(trc::location!())?
        {
            Some(content) => {
                leido = content;
                &leido
            }
            None => return Ok(()),
        },
    };
    let vcard = content
        .unarchive::<ContactCardContent>()
        .caused_by(trc::location!())?
        .card
        .to_string();
    let nombre = propiedad_texto(&vcard, "FN")
        .filter(|n| !n.is_empty())
        .or_else(|| card.display_name.as_ref().map(|n| n.to_string()))
        .unwrap_or_default();
    let item = ElementoArchivado {
        tipo: ArchivedItemType::ContactCard.as_str().to_string(),
        account_id,
        creado: card.created.to_native().max(0) as u64,
        nombre,
        contenedores: card.names.iter().map(|n| n.parent_id.to_native()).collect(),
        nombres: card.names.iter().map(|n| n.name.to_string()).collect(),
        ..Default::default()
    };
    server
        .archive_deleted_item(batch, item, ContenidoArchivado::Datos(vcard.into_bytes()))
        .await
}

/// Archiva un evento que se borra (con su iCalendar completo).
pub async fn archive_event(
    server: &Server,
    batch: &mut BatchBuilder,
    account_id: u32,
    document_id: u32,
    event: &ArchivedCalendarEvent,
    content: Option<&Archive<ArchiveBytes>>,
) -> trc::Result<()> {
    if !activo(server) {
        return Ok(());
    }
    let leido;
    let content = match content {
        Some(content) => content,
        None => match server
            .store()
            .get_value::<Archive<ArchiveBytes>>(ValueKey::property(
                account_id,
                Collection::CalendarEvent,
                document_id,
                CalendarEventField::Content,
            ))
            .await
            .caused_by(trc::location!())?
        {
            Some(content) => {
                leido = content;
                &leido
            }
            None => return Ok(()),
        },
    };
    let ical = content
        .unarchive::<CalendarEventContent>()
        .caused_by(trc::location!())?
        .data
        .event
        .to_string();
    let titulo = propiedad_texto(&ical, "SUMMARY")
        .filter(|n| !n.is_empty())
        .or_else(|| event.display_name.as_ref().map(|n| n.to_string()))
        .unwrap_or_default();
    let item = ElementoArchivado {
        tipo: ArchivedItemType::CalendarEvent.as_str().to_string(),
        account_id,
        creado: event.created.to_native().max(0) as u64,
        nombre: titulo,
        inicio: Some(event.start.to_native()),
        contenedores: event
            .names
            .iter()
            .map(|n| n.parent_id.to_native())
            .collect(),
        nombres: event.names.iter().map(|n| n.name.to_string()).collect(),
        ..Default::default()
    };
    server
        .archive_deleted_item(batch, item, ContenidoArchivado::Datos(ical.into_bytes()))
        .await
}

/// Resultado de una restauración: `Err` con el motivo si no se puede (y el elemento sigue archivado).
pub type Restored = trc::Result<Result<(), String>>;

/// Nombre libre dentro de un contenedor: el original o, si está ocupado, `<base>-<n>.<ext>`.
fn free_name(taken: impl Fn(&str) -> bool, original: &str, fallback: &str) -> String {
    let original = if original.is_empty() {
        fallback
    } else {
        original
    };
    if !taken(original) {
        return original.to_string();
    }
    let (base, ext) = match original.rsplit_once('.') {
        Some((base, ext)) if !base.is_empty() => (base, format!(".{ext}")),
        _ => (original, String::new()),
    };
    (1..1000)
        .map(|n| format!("{base}-{n}{ext}"))
        .find(|name| !taken(name))
        .unwrap_or_else(|| format!("{base}-{}{ext}", now()))
}

fn child_taken(resources: &GroupwareResources, container_id: u32, name: &str) -> bool {
    resources.children(container_id).any(|child| {
        child
            .path()
            .rsplit('/')
            .next()
            .is_some_and(|n| n.eq_ignore_ascii_case(name))
    })
}

fn quota_error(err: &trc::Error) -> Option<String> {
    err.matches(trc::EventType::Limit(trc::LimitEvent::Quota))
        .then(|| "No hay cuota para restaurarlo (de la cuenta o del inquilino)".to_string())
}

/// Restaura un archivo en su carpeta original (si ya no existe, en la raíz).
pub async fn restore_file(
    server: &Server,
    account_id: u32,
    item: &ElementoArchivado,
    hash: BlobHash,
) -> Restored {
    let resources = server
        .fetch_groupware_resources(account_id, account_id, SyncCollection::FileNode)
        .await
        .caused_by(trc::location!())?;
    let account = server.account(account_id).await?;
    if let Err(err) = server.assert_object_quota(&account, StorageQuota::MaxFiles, 1, || {
        resources.resources.count(false)
    }) {
        return Ok(Err(quota_error(&err).unwrap_or_else(|| err.to_string())));
    }
    if let Err(err) = server.has_available_quota(&account, item.tamano).await {
        return match quota_error(&err) {
            Some(reason) => Ok(Err(reason)),
            None => Err(err),
        };
    }

    // Carpeta: la original si sigue existiendo (en `FileNode` el padre es id + 1; 0 es la raíz).
    let parent_id = item
        .contenedores
        .first()
        .copied()
        .filter(|parent| *parent > 0 && resources.has_container_id(&(parent - 1)))
        .unwrap_or(0);
    let siblings: Vec<String> = if parent_id == 0 {
        resources
            .resources
            .iter()
            .filter(|r| r.parent_id().is_none())
            .filter_map(|r| r.container_name().map(str::to_lowercase))
            .collect()
    } else {
        resources
            .children(parent_id - 1)
            .filter_map(|c| c.resource.container_name().map(str::to_lowercase))
            .collect()
    };
    let name = free_name(
        |name| siblings.contains(&name.to_lowercase()),
        &item.nombre,
        "restaurado",
    );
    let media_type = item
        .etiquetas
        .iter()
        .find_map(|e| e.strip_prefix(MIME))
        .map(str::to_string);
    let node = FileNode {
        parent_id,
        name,
        content: FileNodeContent::File(FileProperties {
            blob_hash: hash,
            size: item.tamano as u32,
            media_type,
            executable: item.etiquetas.iter().any(|e| e == EXEC),
        }),
        created: item.creado as i64,
        ..Default::default()
    };

    let mut batch = BatchBuilder::new();
    let document_id = batch.reserve_document_id(account_id, Collection::FileNode);
    batch.with_account_id(account_id);
    server.limit_disk_quota(&mut batch, &account);
    node.insert(
        account.changed_by(),
        account_id,
        document_id,
        false,
        true,
        &mut batch,
    )
    .caused_by(trc::location!())?;
    commit(server, batch).await
}

async fn commit(server: &Server, batch: BatchBuilder) -> Restored {
    match server.commit_batch(batch).await {
        Ok(_) => Ok(Ok(())),
        Err(err) => match quota_error(&err) {
            Some(reason) => Ok(Err(reason)),
            None => Err(err.caused_by(trc::location!())),
        },
    }
}

/// Contenedores donde vuelve un evento o contacto: los originales que sigan existiendo, con su nombre si está
/// libre; si no queda ninguno, `default` (el calendario o la libreta por defecto) con un nombre nuevo.
fn target_names(
    resources: &GroupwareResources,
    item: &ElementoArchivado,
    uid: Option<&str>,
    default: Option<u32>,
    ext: &str,
) -> Result<Vec<DavName>, String> {
    let fallback = format!("{}.{ext}", uid.unwrap_or("restaurado"));
    let mut names = Vec::new();
    for (pos, container_id) in item.contenedores.iter().enumerate() {
        if !resources.has_container_id(container_id)
            || names.iter().any(|n: &DavName| n.parent_id == *container_id)
        {
            continue;
        }
        let original = item
            .nombres
            .get(pos)
            .map(String::as_str)
            .unwrap_or_default();
        names.push(DavName {
            name: free_name(
                |n| child_taken(resources, *container_id, n),
                original,
                &fallback,
            ),
            parent_id: *container_id,
        });
    }
    if names.is_empty() {
        let Some(container_id) = default else {
            return Err("No queda ningún contenedor donde restaurarlo".to_string());
        };
        let original = item.nombres.first().map(String::as_str).unwrap_or_default();
        names.push(DavName {
            name: free_name(
                |n| child_taken(resources, container_id, n),
                original,
                &fallback,
            ),
            parent_id: container_id,
        });
    }
    // Un UID solo puede estar una vez en cada contenedor (RFC 4791 y 6352).
    if let Some(uid) = uid
        && let Some(name) = names.iter().find(|name| {
            resources
                .children_with_uid(name.parent_id, uid)
                .next()
                .is_some()
        })
    {
        return Err(format!(
            "Ya existe un elemento con el mismo UID en el contenedor {}",
            name.parent_id
        ));
    }
    Ok(names)
}

/// Restaura un contacto en sus libretas originales (o en la libreta por defecto).
pub async fn restore_contact(
    server: &Server,
    account_id: u32,
    item: &ElementoArchivado,
    contents: &[u8],
) -> Restored {
    let Ok(vcard) = VCard::parse(std::str::from_utf8(contents).unwrap_or_default()) else {
        return Ok(Err(
            "El contacto archivado no es una vCard válida".to_string()
        ));
    };
    let account = server.account(account_id).await?;
    let mut resources = server
        .fetch_groupware_resources(account_id, account_id, SyncCollection::AddressBook)
        .await
        .caused_by(trc::location!())?;
    let has_container = item
        .contenedores
        .iter()
        .any(|id| resources.has_container_id(id));
    let default = if has_container {
        None
    } else {
        let created = server
            .create_default_addressbook(&account, &account)
            .await
            .caused_by(trc::location!())?;
        resources = refresh(server, account_id, SyncCollection::AddressBook, resources).await?;
        created.or_else(|| resources.document_ids(true).next())
    };
    if let Err(err) = server.assert_object_quota(&account, StorageQuota::MaxContactCards, 1, || {
        resources.resources.count(false)
    }) {
        return Ok(Err(quota_error(&err).unwrap_or_else(|| err.to_string())));
    }
    if let Err(err) = server
        .has_available_quota(&account, contents.len() as u64)
        .await
    {
        return match quota_error(&err) {
            Some(reason) => Ok(Err(reason)),
            None => Err(err),
        };
    }
    let names = match target_names(&resources, item, vcard.uid(), default, "vcf") {
        Ok(names) => names,
        Err(reason) => return Ok(Err(reason)),
    };
    let card = ContactCard {
        names,
        ..Default::default()
    };
    let mut batch = BatchBuilder::new();
    batch.with_account_id(account_id);
    server.limit_disk_quota(&mut batch, &account);
    let document_id = batch.reserve_document_id(account_id, Collection::ContactCard);
    card.insert(
        ContactCardContent { card: vcard },
        server.core.groupware.vcard_version,
        account.changed_by(),
        account_id,
        document_id,
        None,
        &mut batch,
    )
    .caused_by(trc::location!())?;
    commit(server, batch).await
}

async fn refresh(
    server: &Server,
    account_id: u32,
    collection: SyncCollection,
    current: Arc<GroupwareResources>,
) -> trc::Result<Arc<GroupwareResources>> {
    server
        .fetch_groupware_resources_at(
            account_id,
            account_id,
            collection,
            current.highest_change_id + 1,
        )
        .await
        .caused_by(trc::location!())
}

/// Restaura un evento en sus calendarios originales (o en el calendario por defecto), sin enviar invitaciones.
pub async fn restore_event(
    server: &Server,
    account_id: u32,
    item: &ElementoArchivado,
    contents: &[u8],
) -> Restored {
    let Ok(ical) = ICalendar::parse(std::str::from_utf8(contents).unwrap_or_default()) else {
        return Ok(Err(
            "El evento archivado no es un iCalendar válido".to_string()
        ));
    };
    let account = server.account(account_id).await?;
    let mut resources = server
        .fetch_groupware_resources(account_id, account_id, SyncCollection::Calendar)
        .await
        .caused_by(trc::location!())?;
    let has_container = item
        .contenedores
        .iter()
        .any(|id| resources.has_container_id(id));
    let default = if has_container {
        None
    } else {
        let created = server
            .get_or_create_default_calendar(&account, &account)
            .await
            .caused_by(trc::location!())?;
        resources = refresh(server, account_id, SyncCollection::Calendar, resources).await?;
        created
    };
    if let Err(err) =
        server.assert_object_quota(&account, StorageQuota::MaxCalendarEvents, 1, || {
            resources.resources.count(false)
        })
    {
        return Ok(Err(quota_error(&err).unwrap_or_else(|| err.to_string())));
    }
    if let Err(err) = server
        .has_available_quota(&account, contents.len() as u64)
        .await
    {
        return match quota_error(&err) {
            Some(reason) => Ok(Err(reason)),
            None => Err(err),
        };
    }
    let uid = ical.uids().next().map(str::to_string);
    let names = match target_names(&resources, item, uid.as_deref(), default, "ics") {
        Ok(names) => names,
        Err(reason) => return Ok(Err(reason)),
    };
    let calendar_ids = names.iter().map(|n| n.parent_id).collect::<Vec<_>>();
    let event = CalendarEvent {
        names,
        ..Default::default()
    };
    let content = CalendarEventContent {
        data: CalendarEventData::new(ical, Tz::Floating, server.core.groupware.max_ical_instances),
        preferences: Vec::new(),
    };
    let next_alarms = server
        .next_event_alarms(
            account_id,
            &EventAlarmUsers::new(account_id, &content)?,
            &content.data,
            &calendar_ids,
            now() as i64,
            &mut DefaultAlertsResolver::with_resources(account_id, resources.clone()),
        )
        .await
        .caused_by(trc::location!())?;
    let mut batch = BatchBuilder::new();
    batch.with_account_id(account_id);
    server.limit_disk_quota(&mut batch, &account);
    let document_id = batch.reserve_document_id(account_id, Collection::CalendarEvent);
    event
        .insert(
            content,
            account.changed_by(),
            account_id,
            document_id,
            None,
            next_alarms,
            &mut batch,
        )
        .caused_by(trc::location!())?;
    commit(server, batch).await
}
