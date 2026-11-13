/*
 * SPDX-FileCopyrightText: 2026 bezenti
 *
 * SPDX-License-Identifier: AGPL-3.0-only
 */

//! Almacenes repartidos (*sharding*) de la edición bezenti, fase 6: `BlobStore.Sharded` e
//! `InMemoryStore.Sharded` / `LookupStore.Sharded` del esquema.
//!
//! Cada clave vive en UN miembro, elegido con [`super::fragmento`] (rendezvous hashing sobre los bytes de la
//! clave). Los miembros no se replican entre sí y no se rebalancea nada: ver la especificación
//! `bezenti/especificaciones/replicas-y-reparto.md` (qué pasa al añadir miembros, lecturas de respaldo).

use super::fragmento;
use crate::{
    BlobStore, Deserialize, InMemoryStore, Value,
    dispatch::lookup::{KeyValue, LookupKey},
    registry::bootstrap::Bootstrap,
};
use registry::schema::structs;
use std::{future::Future, pin::Pin, sync::Arc};

/// Futuro en caja sin tipo concreto. Los almacenes repartidos llaman a sus miembros, que son del mismo tipo
/// (`BlobStore`, `InMemoryStore`): devolver la caja desde una función normal corta el ciclo de tipos de esa
/// recursión (si no, el compilador no puede comprobar que el futuro es `Send`).
pub type FuturoCaja<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Almacén de blobs repartido entre varios miembros.
pub struct BlobRepartido {
    pub miembros: Box<[BlobStore]>,
}

impl BlobRepartido {
    pub fn nuevo(miembros: Vec<BlobStore>) -> Result<Self, String> {
        if miembros.len() < 2 {
            return Err("A sharded blob store needs at least two stores".to_string());
        }
        Ok(BlobRepartido {
            miembros: miembros.into_boxed_slice(),
        })
    }

    #[inline]
    pub fn indice(&self, key: &[u8]) -> usize {
        fragmento(key, self.miembros.len())
    }

    #[inline]
    pub fn miembro(&self, key: &[u8]) -> &BlobStore {
        &self.miembros[self.indice(key)]
    }

    /// Lectura: del miembro asignado. Si allí no está (`None`), se busca en los demás: así siguen
    /// encontrándose los blobs escritos antes de añadir un miembro nuevo al final de la lista. Un error del
    /// miembro asignado se devuelve tal cual (no hay conmutación: cada miembro debe ser de alta
    /// disponibilidad por sí mismo).
    pub fn get_raw_blob<'a>(
        &'a self,
        key: &'a [u8],
    ) -> FuturoCaja<'a, trc::Result<Option<Vec<u8>>>> {
        Box::pin(async move {
            let propio = self.indice(key);
            if let Some(datos) = self.miembros[propio].get_raw_blob(key).await? {
                return Ok(Some(datos));
            }
            for (i, miembro) in self.miembros.iter().enumerate() {
                if i != propio
                    && let Ok(Some(datos)) = miembro.get_raw_blob(key).await
                {
                    return Ok(Some(datos));
                }
            }
            Ok(None)
        })
    }

    /// Escritura: solo en el miembro asignado.
    pub fn escribir<'a>(&'a self, key: &'a [u8], data: Vec<u8>) -> FuturoCaja<'a, trc::Result<()>> {
        Box::pin(self.miembro(key).write_blob(key, Ok(data)))
    }

    /// Borrado: en el miembro asignado; si allí no estaba, en los demás (mismo motivo que la lectura).
    pub fn delete_blob<'a>(&'a self, key: &'a [u8]) -> FuturoCaja<'a, trc::Result<bool>> {
        Box::pin(async move {
            let propio = self.indice(key);
            if self.miembros[propio].delete_blob(key).await? {
                return Ok(true);
            }
            for (i, miembro) in self.miembros.iter().enumerate() {
                if i != propio && miembro.delete_blob(key).await.unwrap_or(false) {
                    return Ok(true);
                }
            }
            Ok(false)
        })
    }

    /// Borrado en bloque: se agrupan las claves por miembro y cada grupo usa el borrado en bloque del suyo.
    pub fn delete_blobs<'a>(&'a self, keys: &'a [&'a [u8]]) -> FuturoCaja<'a, trc::Result<()>> {
        Box::pin(async move {
            let mut grupos: Vec<Vec<&[u8]>> = vec![Vec::new(); self.miembros.len()];
            for key in keys {
                grupos[self.indice(key)].push(key);
            }
            for (miembro, grupo) in self.miembros.iter().zip(grupos) {
                if !grupo.is_empty() {
                    miembro.delete_blobs(&grupo).await?;
                }
            }
            Ok(())
        })
    }

    /// Construye cada miembro (`x:BlobStoreBase`) con el mismo código que un almacén de blobs suelto.
    #[allow(unreachable_patterns)]
    pub async fn abrir(config: structs::ShardedBlobStore) -> Result<BlobStore, String> {
        let mut miembros = Vec::with_capacity(config.stores.len());
        for (i, miembro) in config.stores.into_iter().enumerate() {
            let abierto = match miembro {
                #[cfg(feature = "foundation")]
                structs::BlobStoreBase::FoundationDb(c) => {
                    crate::backend::foundationdb::FdbStore::open(c)
                        .await
                        .map(BlobStore::Store)
                }
                #[cfg(feature = "postgres")]
                structs::BlobStoreBase::PostgreSql(c) => {
                    crate::backend::postgres::PostgresStore::open(c)
                        .await
                        .map(BlobStore::Store)
                }
                #[cfg(feature = "mysql")]
                structs::BlobStoreBase::MySql(c) => crate::backend::mysql::MysqlStore::open(c)
                    .await
                    .map(BlobStore::Store),
                #[cfg(feature = "s3")]
                structs::BlobStoreBase::S3(c) => crate::backend::s3::S3Store::open(c).await,
                #[cfg(feature = "azure")]
                structs::BlobStoreBase::Azure(c) => {
                    crate::backend::azure::AzureStore::open(c).await
                }
                structs::BlobStoreBase::FileSystem(c) => crate::backend::fs::FsStore::open(c).await,
                _ => {
                    Err("Binary was not compiled with the selected blob store backend".to_string())
                }
            };
            miembros.push(
                abierto.map_err(|err| format!("Sharded blob store member {}: {err}", i + 1))?,
            );
        }
        BlobRepartido::nuevo(miembros).map(|r| BlobStore::Repartido(Arc::new(r)))
    }
}

/// Almacén en memoria (o de búsqueda) repartido entre varios miembros.
pub struct MemoriaRepartida {
    pub miembros: Box<[InMemoryStore]>,
}

impl std::fmt::Debug for MemoriaRepartida {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Sharded")
            .field(&self.miembros.len())
            .finish()
    }
}

impl MemoriaRepartida {
    pub fn nuevo(miembros: Vec<InMemoryStore>) -> Result<Self, String> {
        if miembros.len() < 2 {
            return Err("A sharded in-memory store needs at least two stores".to_string());
        }
        Ok(MemoriaRepartida {
            miembros: miembros.into_boxed_slice(),
        })
    }

    #[inline]
    pub fn miembro(&self, key: &[u8]) -> &InMemoryStore {
        &self.miembros[fragmento(key, self.miembros.len())]
    }

    pub fn key_set(&self, kv: KeyValue<Vec<u8>>) -> FuturoCaja<'_, trc::Result<()>> {
        let miembro = self.miembro(&kv.key);
        Box::pin(miembro.key_set(kv))
    }

    pub fn key_incr_at(&self, kv: KeyValue<i64>, now: u64) -> FuturoCaja<'_, trc::Result<i64>> {
        let miembro = self.miembro(&kv.key);
        Box::pin(miembro.key_incr_at(kv, now))
    }

    pub fn key_delete<'a>(&'a self, key: LookupKey<'a>) -> FuturoCaja<'a, trc::Result<bool>> {
        let miembro = self.miembro(key.as_bytes());
        Box::pin(miembro.key_delete(key))
    }

    pub fn counter_delete<'a>(&'a self, key: LookupKey<'a>) -> FuturoCaja<'a, trc::Result<()>> {
        let miembro = self.miembro(key.as_bytes());
        Box::pin(miembro.counter_delete(key))
    }

    /// Borrar por prefijo: el prefijo no dice en qué miembro están las claves, así que se borra en todos.
    pub fn key_delete_prefix<'a>(&'a self, prefix: &'a [u8]) -> FuturoCaja<'a, trc::Result<()>> {
        Box::pin(async move {
            for miembro in self.miembros.iter() {
                miembro.key_delete_prefix(prefix).await?;
            }
            Ok(())
        })
    }

    pub fn key_get<'a, T>(&'a self, key: LookupKey<'a>) -> FuturoCaja<'a, trc::Result<Option<T>>>
    where
        T: Deserialize + From<Value<'static>> + std::fmt::Debug + 'static,
    {
        let miembro = self.miembro(key.as_bytes());
        Box::pin(miembro.key_get(key))
    }

    pub fn key_exists<'a>(&'a self, key: LookupKey<'a>) -> FuturoCaja<'a, trc::Result<bool>> {
        let miembro = self.miembro(key.as_bytes());
        Box::pin(miembro.key_exists(key))
    }

    /// Cerrojos: el miembro se elige por la clave completa (`prefijo + clave`), la misma que usa
    /// `remove_lock` al borrarla.
    pub fn try_lock<'a>(
        &'a self,
        ruta: &[u8],
        prefix: u8,
        key: &'a [u8],
        duration: u64,
    ) -> FuturoCaja<'a, trc::Result<bool>> {
        let miembro = self.miembro(ruta);
        Box::pin(miembro.try_lock(prefix, key, duration))
    }

    pub fn renew_lock<'a>(
        &'a self,
        ruta: &[u8],
        prefix: u8,
        key: &'a [u8],
        duration: u64,
    ) -> FuturoCaja<'a, trc::Result<()>> {
        let miembro = self.miembro(ruta);
        Box::pin(miembro.renew_lock(prefix, key, duration))
    }

    /// Arriendos: `ruta` es la clave completa (`prefijo + clave`).
    pub fn try_lease<'a>(
        &'a self,
        ruta: &'a [u8],
        prefix: u8,
        token: u64,
        duration: u64,
    ) -> FuturoCaja<'a, trc::Result<bool>> {
        let miembro = self.miembro(ruta);
        Box::pin(miembro.try_lease(prefix, &ruta[1..], token, duration))
    }

    pub fn renew_lease<'a>(
        &'a self,
        ruta: &'a [u8],
        prefix: u8,
        token: u64,
        duration: u64,
    ) -> FuturoCaja<'a, trc::Result<bool>> {
        let miembro = self.miembro(ruta);
        Box::pin(miembro.renew_lease(prefix, &ruta[1..], token, duration))
    }

    pub fn release_lease<'a>(
        &'a self,
        ruta: &'a [u8],
        prefix: u8,
        token: u64,
    ) -> FuturoCaja<'a, trc::Result<bool>> {
        let miembro = self.miembro(ruta);
        Box::pin(miembro.release_lease(prefix, &ruta[1..], token))
    }

    /// Construye cada miembro (`x:InMemoryStoreBase`: Redis, Redis Cluster o Redis Sentinel).
    #[allow(unreachable_patterns, unused_variables)]
    pub async fn abrir(
        _bp: &mut Bootstrap,
        config: structs::ShardedInMemoryStore,
    ) -> Result<InMemoryStore, String> {
        let mut miembros = Vec::with_capacity(config.stores.len());
        for (i, miembro) in config.stores.into_iter().enumerate() {
            let abierto: Result<InMemoryStore, String> = match miembro {
                #[cfg(feature = "redis")]
                structs::InMemoryStoreBase::Redis(c) => {
                    crate::backend::redis::RedisStore::open_single(c).await
                }
                #[cfg(feature = "redis")]
                structs::InMemoryStoreBase::RedisCluster(c) => {
                    crate::backend::redis::RedisStore::open_cluster(c).await
                }
                #[cfg(feature = "redis")]
                structs::InMemoryStoreBase::RedisSentinel(c) => {
                    crate::backend::redis::RedisStore::open_sentinel(c).await
                }
                _ => Err("Binary was not compiled with the selected in-memory backend".to_string()),
            };
            miembros.push(
                abierto
                    .map_err(|err| format!("Sharded in-memory store member {}: {err}", i + 1))?,
            );
        }
        MemoriaRepartida::nuevo(miembros).map(|r| InMemoryStore::Repartido(Arc::new(r)))
    }
}

#[cfg(test)]
mod pruebas {
    use super::*;
    use crate::backend::{fs::FsStore, local::LocalStore};

    async fn fs(dir: &std::path::Path) -> BlobStore {
        FsStore::open(structs::FileSystemStore {
            path: dir.to_string_lossy().into_owned(),
            depth: 1,
        })
        .await
        .unwrap()
    }

    async fn varios(dirs: &[std::path::PathBuf]) -> Vec<BlobStore> {
        let mut v = Vec::new();
        for d in dirs {
            v.push(fs(d).await);
        }
        v
    }

    fn contar_archivos(dir: &std::path::Path) -> usize {
        let mut n = 0;
        let mut pila = vec![dir.to_path_buf()];
        while let Some(d) = pila.pop() {
            for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
                let p = e.path();
                if p.is_dir() {
                    pila.push(p);
                } else {
                    n += 1;
                }
            }
        }
        n
    }

    #[tokio::test]
    async fn blobs_en_su_fragmento() {
        let raiz = std::env::temp_dir().join(format!("bezenti-reparto-{}", std::process::id()));
        let dirs: Vec<_> = (0..3).map(|i| raiz.join(format!("f{i}"))).collect();
        for d in &dirs {
            std::fs::create_dir_all(d).unwrap();
        }
        let repartido = BlobRepartido::nuevo(varios(&dirs).await).unwrap();
        let blob = BlobStore::Repartido(Arc::new(repartido));
        let BlobStore::Repartido(r) = &blob else {
            unreachable!()
        };

        let total = 300u32;
        let claves: Vec<Vec<u8>> = (0..total)
            .map(|i| blake3::hash(&i.to_le_bytes()).as_bytes().to_vec())
            .collect();
        for (i, k) in claves.iter().enumerate() {
            blob.put_blob(
                k,
                format!("blob {i}").as_bytes(),
                &crate::write::blob_codec::BlobOpts::PLAIN,
            )
            .await
            .unwrap();
        }

        // Cada blob está en el directorio de su fragmento, y solo en ése.
        for (i, k) in claves.iter().enumerate() {
            let propio = r.indice(k);
            for (j, m) in r.miembros.iter().enumerate() {
                let esta = m.get_raw_blob(k).await.unwrap().is_some();
                assert_eq!(esta, j == propio, "blob {i}: fragmento {j}");
            }
            let leido = blob
                .get_blob(k, 0..usize::MAX, &Default::default())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(leido, format!("blob {i}").as_bytes());
        }

        // Reparto razonable: cada directorio tiene entre el 25 % y el 42 % de los blobs.
        let por_dir: Vec<usize> = dirs.iter().map(|d| contar_archivos(d)).collect();
        assert_eq!(por_dir.iter().sum::<usize>(), total as usize);
        for n in &por_dir {
            assert!(
                (total as usize * 25 / 100..=total as usize * 42 / 100).contains(n),
                "reparto: {por_dir:?}"
            );
        }

        // Borrado: desaparece de su fragmento.
        for k in claves.iter().take(100) {
            assert!(blob.delete_blob(k).await.unwrap());
        }
        let refs: Vec<&[u8]> = claves[100..200].iter().map(|k| k.as_slice()).collect();
        blob.delete_blobs(&refs).await.unwrap();
        for k in claves.iter().take(200) {
            assert!(blob.get_raw_blob(k).await.unwrap().is_none());
        }
        let quedan: usize = dirs.iter().map(|d| contar_archivos(d)).sum();
        assert_eq!(quedan, 100);

        // Añadir un cuarto fragmento al final: lo viejo se sigue leyendo (lectura de respaldo) y borrando.
        let d4 = raiz.join("f3");
        std::fs::create_dir_all(&d4).unwrap();
        let mut miembros = varios(&dirs).await;
        miembros.push(fs(&d4).await);
        let ampliado = BlobStore::Repartido(Arc::new(BlobRepartido::nuevo(miembros).unwrap()));
        let BlobStore::Repartido(a) = &ampliado else {
            unreachable!()
        };
        let mut movidos = 0;
        for (i, k) in claves.iter().enumerate().skip(200) {
            if a.indice(k) == 3 {
                movidos += 1;
            }
            let leido = ampliado
                .get_blob(k, 0..usize::MAX, &Default::default())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(leido, format!("blob {i}").as_bytes());
        }
        assert!(movidos > 0 && movidos < 50, "movidos al nuevo: {movidos}");
        for k in claves.iter().skip(200) {
            assert!(ampliado.delete_blob(k).await.unwrap());
        }
        assert_eq!(dirs.iter().map(|d| contar_archivos(d)).sum::<usize>(), 0);
        assert!(!ampliado.delete_blob(&claves[250]).await.unwrap());

        std::fs::remove_dir_all(&raiz).ok();
    }

    #[tokio::test]
    async fn memoria_repartida() {
        let locales: Vec<Arc<LocalStore>> =
            (0..3).map(|_| Arc::new(LocalStore::default())).collect();
        let memoria = InMemoryStore::Repartido(Arc::new(
            MemoriaRepartida::nuevo(
                locales
                    .iter()
                    .map(|l| InMemoryStore::Local(l.clone()))
                    .collect(),
            )
            .unwrap(),
        ));
        let InMemoryStore::Repartido(r) = &memoria else {
            unreachable!()
        };

        for i in 0..300u32 {
            memoria
                .key_set(KeyValue::new(
                    format!("clave-{i}").into_bytes(),
                    format!("valor-{i}").into_bytes(),
                ))
                .await
                .unwrap();
        }
        let mut por_miembro = [0usize; 3];
        for i in 0..300u32 {
            let clave = format!("clave-{i}");
            let propio = crate::backend::bezenti::fragmento(clave.as_bytes(), 3);
            por_miembro[propio] += 1;
            for (j, m) in r.miembros.iter().enumerate() {
                assert_eq!(
                    m.key_exists(clave.as_str()).await.unwrap(),
                    j == propio,
                    "{clave} en el miembro {j}"
                );
            }
            let v: Option<String> = memoria.key_get(clave.as_str()).await.unwrap();
            assert_eq!(v.unwrap(), format!("valor-{i}"));
        }
        assert!(por_miembro.iter().all(|n| *n > 60), "{por_miembro:?}");

        // Contadores, borrados, prefijos y cerrojos.
        assert_eq!(
            memoria
                .key_incr(KeyValue::new(b"cnt".to_vec(), 2))
                .await
                .unwrap(),
            2
        );
        assert_eq!(
            memoria
                .key_incr(KeyValue::new(b"cnt".to_vec(), 3))
                .await
                .unwrap(),
            5
        );
        assert!(memoria.key_delete("clave-1").await.unwrap());
        assert!(!memoria.key_exists("clave-1").await.unwrap());
        memoria.key_delete_prefix(b"clave-").await.unwrap();
        for i in 0..300u32 {
            assert!(
                !memoria
                    .key_exists(format!("clave-{i}").as_str())
                    .await
                    .unwrap()
            );
        }
        assert!(memoria.try_lock(1, b"cerrojo", 60).await.unwrap());
        assert!(!memoria.try_lock(1, b"cerrojo", 60).await.unwrap());
        memoria.remove_lock(1, b"cerrojo").await.unwrap();
        assert!(memoria.try_lock(1, b"cerrojo", 60).await.unwrap());
        assert!(memoria.try_lease(2, b"arriendo", 7, 60).await.unwrap());
        assert!(!memoria.try_lease(2, b"arriendo", 8, 60).await.unwrap());
        assert!(memoria.release_lease(2, b"arriendo", 7).await.unwrap());
        assert!(
            memoria
                .is_rate_allowed(
                    3,
                    b"ritmo",
                    &registry::schema::structs::Rate {
                        count: 1,
                        period: std::time::Duration::from_secs(60).into(),
                    }
                )
                .await
                .unwrap()
                .is_none()
        );
    }
}
