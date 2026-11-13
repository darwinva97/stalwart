/*
 * SPDX-FileCopyrightText: 2026 bezenti
 *
 * SPDX-License-Identifier: AGPL-3.0-only
 */

//! Archivo de elementos borrados (`x:ArchivedItem`). Edición bezenti, fase 3.
//!
//! Especificación: `bezenti/especificaciones/archivo.md`. Cómo se guarda (§5 de la especificación):
//!
//! - Cada elemento archivado es un registro propio (`ElementoArchivado`, JSON compacto) en el subespacio
//!   `DeletedItems` del almacén, con la clave de registro `(x:ArchivedItem, id)` y el índice `accountId` del
//!   registro (el mismo esquema que las muestras de entrenamiento antispam, así funcionan `query` y la purga).
//! - El contenido (el mensaje, el guion…) no se copia: el blob sigue en el almacén de blobs gracias a un **enlace
//!   temporal** `(hash, cuenta, archivedUntil)` cuyo valor apunta al elemento. Cuando vence, la limpieza de blobs
//!   programada (`blobCleanupSchedule`, o la tarea `StoreMaintenance`/`purgeBlob`) borra el enlace, el elemento y
//!   su índice, y el blob si nadie más lo usa. Esa es la «purga al vencer».
//! - Un mensaje borrado se archiva en dos pasos: al borrarlo se apunta un «pendiente» con lo que se pierde al
//!   borrar (buzones y etiquetas, ver [`EmailPendiente`]); cuando la cola del índice quita sus metadatos (que es
//!   cuando el servidor suelta el blob) se crea el elemento con el remitente, el asunto y el tamaño.

use crate::Server;
use registry::{
    schema::{
        enums::{ArchivedItemStatus, ArchivedItemType},
        prelude::{ObjectType, Property},
        structs::{
            ArchivedCalendarEvent, ArchivedContactCard, ArchivedEmail, ArchivedFileNode,
            ArchivedItem, ArchivedSieveScript,
        },
    },
    types::{EnumImpl, datetime::UTCDateTime, id::ObjectId},
};
use serde::{Deserialize, Serialize};
use store::{
    IterateParams, SerializeInfallible, Subspace, U32_LEN, ValueKey,
    write::{
        AnyClass, BatchBuilder, BlobLink, BlobOp, RegistryClass, ValueClass, blob::BlobLinkValue,
    },
};
use trc::AddContext;
use types::{
    blob::{BlobClass, BlobId},
    blob_hash::BlobHash,
    id::Id,
};

/// Marca de las claves de «pendiente de archivar» dentro de `DeletedItems` (las de los elementos empiezan por
/// el id de objeto `x:ArchivedItem`, que nunca es `u16::MAX`).
const MARCA_PENDIENTE: [u8; 2] = [0xFF, 0xFF];

/// Un elemento del archivo tal como se guarda.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ElementoArchivado {
    /// Tipo (`Email`, `FileNode`, `CalendarEvent`, `ContactCard`, `SieveScript`).
    #[serde(rename = "t")]
    pub tipo: String,
    #[serde(rename = "a")]
    pub account_id: u32,
    /// Hash del blob, en hexadecimal.
    #[serde(rename = "h")]
    pub hash: String,
    #[serde(rename = "s")]
    pub tamano: u64,
    #[serde(rename = "aa")]
    pub archivado: u64,
    #[serde(rename = "au")]
    pub hasta: u64,
    /// Correo: `receivedAt`; resto: `createdAt`.
    #[serde(rename = "c")]
    pub creado: u64,
    /// Se ha pedido la restauración (`status: requestRestore`).
    #[serde(rename = "r", default)]
    pub restaurar: bool,
    /// Asunto (correo), nombre (archivo, contacto, guion) o título (evento).
    #[serde(rename = "n", default)]
    pub nombre: String,
    /// Remitente (correo).
    #[serde(rename = "f", default)]
    pub de: String,
    /// Inicio del evento (calendario).
    #[serde(rename = "i", default)]
    pub inicio: Option<i64>,
    /// Contenedores originales: buzones (correo), libretas, calendarios o carpeta.
    #[serde(rename = "m", default)]
    pub contenedores: Vec<u32>,
    /// Etiquetas originales (correo) o marcas (`active` en un guion Sieve).
    #[serde(rename = "k", default)]
    pub etiquetas: Vec<String>,
    /// Correo: cabeceras completas del mensaje (en base64) cuando el servidor añadió cabeceras al recibirlo
    /// (`Received`, `Delivered-To`…), que se guardan aparte del blob; con `cuerpo` (dónde empieza el cuerpo en el
    /// blob) se reconstruye el mensaje íntegro: `cabeceras + blob[cuerpo..]`.
    #[serde(rename = "hd", default, skip_serializing_if = "Option::is_none")]
    pub cabeceras: Option<String>,
    #[serde(rename = "bo", default)]
    pub cuerpo: usize,
}

/// Lo que se apunta al borrar un mensaje y se pierde con el borrado: dónde estaba y con qué etiquetas.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmailPendiente {
    #[serde(rename = "u")]
    pub hasta: u64,
    #[serde(rename = "m", default)]
    pub buzones: Vec<u32>,
    #[serde(rename = "k", default)]
    pub etiquetas: Vec<String>,
    #[serde(rename = "c", default)]
    pub recibido: u64,
}

impl ElementoArchivado {
    pub fn tipo(&self) -> ArchivedItemType {
        ArchivedItemType::parse(&self.tipo).unwrap_or(ArchivedItemType::Email)
    }

    pub fn blob_hash(&self) -> BlobHash {
        hex_a_hash(&self.hash).unwrap_or_default()
    }

    pub fn con_hash(mut self, hash: &BlobHash) -> Self {
        self.hash = hash.to_hex();
        self
    }

    pub fn serializar(&self) -> Vec<u8> {
        serde_json::to_vec(self).unwrap_or_default()
    }

    /// Guarda las cabeceras completas de un mensaje cuyo blob no las lleva todas.
    pub fn con_cabeceras(mut self, cabeceras: &[u8], cuerpo: usize) -> Self {
        use encodify::base64::STANDARD;
        self.cabeceras = Some(STANDARD.encode(cabeceras));
        self.cuerpo = cuerpo;
        self
    }

    /// El mensaje íntegro a partir del blob archivado.
    pub fn mensaje_integro(&self, blob: Vec<u8>) -> Vec<u8> {
        use encodify::base64::STANDARD;
        match self
            .cabeceras
            .as_deref()
            .and_then(|c| STANDARD.decode(c).ok())
        {
            Some(mut cabeceras) => {
                cabeceras.extend_from_slice(blob.get(self.cuerpo..).unwrap_or_default());
                cabeceras
            }
            None => blob,
        }
    }

    pub fn deserializar(bytes: &[u8]) -> trc::Result<Self> {
        serde_json::from_slice(bytes).map_err(|err| {
            trc::StoreEvent::DataCorruption
                .into_err()
                .details("Elemento archivado ilegible")
                .reason(err)
                .caused_by(trc::location!())
        })
    }

    /// El enlace temporal que mantiene vivo el blob mientras dure el archivo.
    pub fn enlace(&self) -> BlobOp {
        BlobOp::Link {
            hash: self.blob_hash(),
            to: BlobLink::Temporary { until: self.hasta },
        }
    }

    /// `blobId` que se da por la API: descargable por la cuenta dueña mientras exista el enlace temporal.
    pub fn blob_id(&self) -> BlobId {
        BlobId {
            hash: self.blob_hash(),
            class: BlobClass::Reserved {
                account_id: self.account_id,
                expires: self.hasta,
            },
            section: None,
        }
    }

    /// El objeto de la API (`x:ArchivedItem`).
    pub fn a_objeto(&self) -> ArchivedItem {
        let account_id = Id::from(self.account_id);
        let blob_id = self.blob_id();
        let archived_at = fecha(self.archivado);
        let archived_until = fecha(self.hasta);
        let status = if self.restaurar {
            ArchivedItemStatus::RequestRestore
        } else {
            ArchivedItemStatus::Archived
        };
        let created_at = fecha(self.creado);
        match self.tipo() {
            ArchivedItemType::Email => ArchivedItem::Email(ArchivedEmail {
                from: self.de.clone(),
                subject: self.nombre.clone(),
                received_at: created_at,
                size: self.tamano,
                blob_id,
                account_id,
                archived_at,
                archived_until,
                status,
            }),
            ArchivedItemType::FileNode => ArchivedItem::FileNode(ArchivedFileNode {
                name: self.nombre.clone(),
                created_at,
                account_id,
                blob_id,
                archived_at,
                archived_until,
                status,
            }),
            ArchivedItemType::CalendarEvent => ArchivedItem::CalendarEvent(ArchivedCalendarEvent {
                title: self.nombre.clone(),
                start_time: self.inicio.map(UTCDateTime::from_timestamp),
                created_at,
                account_id,
                blob_id,
                archived_at,
                archived_until,
                status,
            }),
            ArchivedItemType::ContactCard => ArchivedItem::ContactCard(ArchivedContactCard {
                name: (!self.nombre.is_empty()).then(|| self.nombre.clone()),
                created_at,
                account_id,
                blob_id,
                archived_at,
                archived_until,
                status,
            }),
            ArchivedItemType::SieveScript => ArchivedItem::SieveScript(ArchivedSieveScript {
                name: self.nombre.clone(),
                created_at,
                contents: String::new(),
                account_id,
                blob_id,
                archived_at,
                archived_until,
                status,
            }),
        }
    }

    /// Escribe el elemento (registro, índice por cuenta y enlace temporal del blob).
    pub fn escribir(&self, batch: &mut BatchBuilder, item_id: u64) {
        batch
            .with_account_id(self.account_id)
            .set(
                self.enlace(),
                BlobLinkValue::new(self.tamano)
                    .with_object_id(ObjectId::new(ObjectType::ArchivedItem, item_id.into())),
            )
            .set(clave_indice(self.account_id, item_id), vec![])
            .set(clave_item(item_id), self.serializar());
    }

    /// Borra el elemento del archivo (el blob se libera en la próxima limpieza si nadie más lo usa).
    pub fn borrar(&self, batch: &mut BatchBuilder, item_id: u64) {
        batch
            .with_account_id(self.account_id)
            .clear(self.enlace())
            .clear(clave_indice(self.account_id, item_id))
            .clear(clave_item(item_id));
    }
}

fn fecha(segundos: u64) -> UTCDateTime {
    UTCDateTime::from_timestamp(segundos as i64)
}

fn hex_a_hash(hex: &str) -> Option<BlobHash> {
    let bytes = (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(hex.get(i..i + 2)?, 16).ok())
        .collect::<Option<Vec<u8>>>()?;
    BlobHash::try_from_hash_slice(&bytes).ok()
}

/// Clave del registro de un elemento archivado.
pub fn clave_item(item_id: u64) -> ValueClass {
    ValueClass::Registry(RegistryClass::Item {
        object_id: ObjectType::ArchivedItem.to_id(),
        item_id,
    })
}

/// Clave del índice por cuenta de un elemento archivado.
pub fn clave_indice(account_id: u32, item_id: u64) -> ValueClass {
    ValueClass::Registry(RegistryClass::Index {
        index_id: Property::AccountId.to_id(),
        object_id: ObjectType::ArchivedItem.to_id(),
        item_id,
        key: SerializeInfallible::serialize(&(account_id as u64)),
    })
}

fn bytes_pendiente(account_id: u32, document_id: u32) -> Vec<u8> {
    let mut key = Vec::with_capacity(2 + U32_LEN * 2);
    key.extend_from_slice(&MARCA_PENDIENTE);
    key.extend_from_slice(&account_id.to_be_bytes());
    key.extend_from_slice(&document_id.to_be_bytes());
    key
}

/// Clave del «pendiente de archivar» de un mensaje borrado.
pub fn clave_pendiente(account_id: u32, document_id: u32) -> ValueClass {
    ValueClass::Any(AnyClass {
        subspace: Subspace::DeletedItems,
        key: bytes_pendiente(account_id, document_id),
    })
}

impl EmailPendiente {
    pub fn serializar(&self) -> Vec<u8> {
        serde_json::to_vec(self).unwrap_or_default()
    }
}

impl Server {
    /// Segundos de retención del archivo (`x:DataRetention.archiveDeletedItemsFor`); `None` = no se archiva.
    #[inline]
    pub fn archive_retention(&self) -> Option<u64> {
        self.core.email.archive_deleted_items_for
    }

    /// Lee un elemento archivado.
    pub async fn archived_item(&self, item_id: u64) -> trc::Result<Option<ElementoArchivado>> {
        match self
            .store()
            .get_value::<store::write::serialize::RawValue>(ValueKey::from(clave_item(item_id)))
            .await
            .caused_by(trc::location!())?
        {
            Some(raw) => ElementoArchivado::deserializar(&raw.0).map(Some),
            None => Ok(None),
        }
    }

    /// Busca un elemento por su enlace temporal (es lo que guarda la tarea `RestoreArchivedItem`).
    pub async fn archived_item_by_link(
        &self,
        account_id: u32,
        hash: &BlobHash,
        until: u64,
    ) -> trc::Result<Option<(u64, ElementoArchivado)>> {
        let key = ValueKey {
            account_id,
            collection: 0,
            document_id: 0,
            class: ValueClass::Blob(BlobOp::Link {
                hash: hash.clone(),
                to: BlobLink::Temporary { until },
            }),
        };
        let Some(link) = self
            .store()
            .get_value::<BlobLinkValue>(key)
            .await
            .caused_by(trc::location!())?
        else {
            return Ok(None);
        };
        let Some(object_id) = link
            .object_id
            .filter(|object_id| object_id.object() == ObjectType::ArchivedItem)
        else {
            return Ok(None);
        };
        let item_id = object_id.id().id();
        Ok(self
            .archived_item(item_id)
            .await?
            .filter(|item| item.account_id == account_id)
            .map(|item| (item_id, item)))
    }

    /// Crea un elemento en el archivo dentro de `batch` y devuelve su id. Si ya hay otro elemento de la misma
    /// cuenta con el mismo blob y la misma fecha de vencimiento (dos copias del mismo mensaje borradas en el
    /// mismo segundo), se adelanta el vencimiento un segundo para que cada uno tenga su enlace.
    pub async fn archive_item_create(
        &self,
        batch: &mut BatchBuilder,
        mut item: ElementoArchivado,
    ) -> trc::Result<u64> {
        let hash = item.blob_hash();
        for _ in 0..60 {
            let key = ValueKey {
                account_id: item.account_id,
                collection: 0,
                document_id: 0,
                class: ValueClass::Blob(BlobOp::Link {
                    hash: hash.clone(),
                    to: BlobLink::Temporary { until: item.hasta },
                }),
            };
            if !self
                .store()
                .key_exists(key)
                .await
                .caused_by(trc::location!())?
            {
                break;
            }
            item.hasta += 1;
        }
        let item_id = self.registry().assign_id();
        item.escribir(batch, item_id);
        Ok(item_id)
    }

    /// Lee y borra (en `batch`) el «pendiente de archivar» de un mensaje.
    pub async fn take_pending_email_archive(
        &self,
        batch: &mut BatchBuilder,
        account_id: u32,
        document_id: u32,
    ) -> trc::Result<Option<EmailPendiente>> {
        let class = clave_pendiente(account_id, document_id);
        match self
            .store()
            .get_value::<store::write::serialize::RawValue>(ValueKey::from(class.clone()))
            .await
            .caused_by(trc::location!())?
        {
            Some(raw) => {
                batch.clear(class);
                Ok(serde_json::from_slice(&raw.0).ok())
            }
            None => Ok(None),
        }
    }

    /// Borra todos los «pendientes de archivar» de una cuenta (al destruirla).
    pub async fn clear_pending_email_archive(&self, account_id: u32) -> trc::Result<()> {
        let from = bytes_pendiente(account_id, 0);
        let to = bytes_pendiente(account_id, u32::MAX);
        let mut keys = Vec::new();
        self.store()
            .iterate(
                IterateParams::new(
                    store::write::AnyKey {
                        subspace: Subspace::DeletedItems,
                        key: from,
                    },
                    store::write::AnyKey {
                        subspace: Subspace::DeletedItems,
                        key: to,
                    },
                )
                .ascending()
                .no_values(),
                |key, _| {
                    keys.push(key.to_vec());
                    Ok(true)
                },
            )
            .await
            .caused_by(trc::location!())?;
        for chunk in keys.chunks(1000) {
            let mut batch = BatchBuilder::new();
            for key in chunk {
                batch.clear(ValueClass::Any(AnyClass {
                    subspace: Subspace::DeletedItems,
                    key: key.clone(),
                }));
            }
            self.store()
                .write_batch(&mut batch)
                .await
                .caused_by(trc::location!())?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ida_y_vuelta() {
        let hash = BlobHash::try_from_hash_slice(&[7u8; 32]).unwrap();
        let item = ElementoArchivado {
            tipo: ArchivedItemType::Email.as_str().to_string(),
            account_id: 3,
            tamano: 1234,
            archivado: 1_700_000_000,
            hasta: 1_700_086_400,
            creado: 1_699_999_000,
            nombre: "Hola".into(),
            de: "a@b.c".into(),
            contenedores: vec![0, 5],
            etiquetas: vec!["$seen".into()],
            ..Default::default()
        }
        .con_hash(&hash);
        let leido = ElementoArchivado::deserializar(&item.serializar()).unwrap();
        assert_eq!(leido, item);
        assert_eq!(leido.blob_hash(), hash);
        assert_eq!(leido.tipo(), ArchivedItemType::Email);
        match leido.a_objeto() {
            ArchivedItem::Email(email) => {
                assert_eq!(email.subject, "Hola");
                assert_eq!(email.size, 1234);
                assert_eq!(email.status, ArchivedItemStatus::Archived);
            }
            _ => panic!("tipo incorrecto"),
        }
        assert_eq!(bytes_pendiente(1, 2).len(), 10);
    }
}
