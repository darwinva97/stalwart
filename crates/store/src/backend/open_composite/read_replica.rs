/*
 * SPDX-FileCopyrightText: 2026 Stalwart contributors
 *
 * SPDX-License-Identifier: AGPL-3.0-only
 *
 * Read-replica store: routes reads to a round-robin replica (falling back to
 * the primary on a non-fatal error) and writes to the primary. Delegates to
 * the inner stores' public `Store` API, so it composes with any backend.
 *
 * Delegating futures are boxed: because the dispatch layer routes a
 * `Store::ReadReplica` back into these methods, the call graph is recursive at
 * the type level even though the inner primary/replicas are always concrete
 * backends at runtime.
 */

use crate::{
    Deserialize, IterateParams, Key, Store, ValueKey,
    write::{AssignedIds, Batch, ValueClass},
};
use std::{
    ops::Range,
    sync::atomic::{AtomicUsize, Ordering},
};

/// A primary store with zero or more read replicas.
pub struct OpenReadReplica {
    primary: Store,
    replicas: Vec<Store>,
    next: AtomicUsize,
}

impl OpenReadReplica {
    pub fn new(primary: Store, replicas: Vec<Store>) -> Self {
        OpenReadReplica {
            primary,
            replicas,
            next: AtomicUsize::new(0),
        }
    }

    /// The authoritative (write) store.
    pub fn primary_store(&self) -> &Store {
        &self.primary
    }

    /// Picks the next replica index (round-robin), or `None` if there are no
    /// replicas.
    fn next_replica(&self) -> Option<usize> {
        if self.replicas.is_empty() {
            None
        } else {
            Some(self.next.fetch_add(1, Ordering::Relaxed) % self.replicas.len())
        }
    }

    /// Read: served by a round-robin replica, falling back to the primary on a
    /// non-fatal error (assertion failures short-circuit). Delegates to the
    /// inner concrete backends so the call graph is not recursive.
    pub async fn get_value<U>(&self, key: impl Key) -> trc::Result<Option<U>>
    where
        U: Deserialize + 'static,
    {
        if let Some(idx) = self.next_replica() {
            match Self::concrete_get_value::<U>(&self.replicas[idx], key.clone()).await {
                Ok(value) => return Ok(value),
                Err(err) if err.is_assertion_failure() => return Err(err),
                Err(_) => {}
            }
        }
        Self::concrete_get_value::<U>(&self.primary, key).await
    }

    async fn concrete_get_value<U: Deserialize + 'static>(
        store: &Store,
        key: impl Key,
    ) -> trc::Result<Option<U>> {
        match store {
            #[cfg(feature = "sqlite")]
            Store::SQLite(s) => s.get_value(key).await,
            #[cfg(feature = "foundation")]
            Store::FoundationDb(s) => s.get_value(key).await,
            #[cfg(feature = "postgres")]
            Store::PostgreSQL(s) => s.get_value(key).await,
            #[cfg(feature = "mysql")]
            Store::MySQL(s) => s.get_value(key).await,
            #[cfg(feature = "rocks")]
            Store::RocksDb(s) => s.get_value(key).await,
            Store::Ephemeral(s) => s.get_value(key).await,
            _ => Err(trc::StoreEvent::NotConfigured.into()),
        }
    }

    /// Read: served by a round-robin replica, falling back to the primary.
    pub async fn key_exists(&self, key: impl Key) -> trc::Result<bool> {
        if let Some(idx) = self.next_replica() {
            match Self::concrete_key_exists(&self.replicas[idx], key.clone()).await {
                Ok(value) => return Ok(value),
                Err(err) if err.is_assertion_failure() => return Err(err),
                Err(_) => {}
            }
        }
        Self::concrete_key_exists(&self.primary, key).await
    }

    async fn concrete_key_exists(store: &Store, key: impl Key) -> trc::Result<bool> {
        match store {
            #[cfg(feature = "sqlite")]
            Store::SQLite(s) => s.key_exists(key).await,
            #[cfg(feature = "foundation")]
            Store::FoundationDb(s) => s.key_exists(key).await,
            #[cfg(feature = "postgres")]
            Store::PostgreSQL(s) => s.key_exists(key).await,
            #[cfg(feature = "mysql")]
            Store::MySQL(s) => s.key_exists(key).await,
            #[cfg(feature = "rocks")]
            Store::RocksDb(s) => s.key_exists(key).await,
            Store::Ephemeral(s) => s.key_exists(key).await,
            _ => Err(trc::StoreEvent::NotConfigured.into()),
        }
    }

    /// Counter read: routed to the primary for a consistent value.
    pub async fn get_counter(
        &self,
        key: impl Into<ValueKey<ValueClass>> + Sync + Send,
    ) -> trc::Result<i64> {
        Box::pin(self.primary.get_counter(key)).await
    }

    /// Iteration runs on the primary: its callback is `FnMut` and cannot be
    /// safely retried against another store.
    pub async fn iterate<T: Key>(
        &self,
        params: IterateParams<T>,
        cb: impl for<'x> FnMut(&'x [u8], &'x [u8]) -> trc::Result<bool> + Sync + Send,
    ) -> trc::Result<()> {
        Box::pin(self.primary.iterate(params, cb)).await
    }

    /// Write: always the primary.
    pub async fn write(&self, batch: Batch<'_>) -> trc::Result<AssignedIds> {
        Box::pin(self.primary.write(batch)).await
    }

    /// Write: always the primary.
    pub async fn delete_range(&self, from: impl Key, to: impl Key) -> trc::Result<()> {
        Box::pin(self.primary.delete_range(from, to)).await
    }

    /// Write: always the primary.
    pub async fn purge_store(&self) -> trc::Result<()> {
        Box::pin(self.primary.purge_store()).await
    }

    // Blob operations delegate to the primary's concrete backend (the outer
    // BlobStore dispatch handles (de)compression). Blobs are not offloaded to
    // replicas, keeping the authoritative store as the single source of truth.

    pub async fn get_blob(&self, key: &[u8], range: Range<usize>) -> trc::Result<Option<Vec<u8>>> {
        match &self.primary {
            #[cfg(feature = "sqlite")]
            Store::SQLite(s) => s.get_blob(key, range).await,
            #[cfg(feature = "foundation")]
            Store::FoundationDb(s) => s.get_blob(key, range).await,
            #[cfg(feature = "postgres")]
            Store::PostgreSQL(s) => s.get_blob(key, range).await,
            #[cfg(feature = "mysql")]
            Store::MySQL(s) => s.get_blob(key, range).await,
            #[cfg(feature = "rocks")]
            Store::RocksDb(s) => s.get_blob(key, range).await,
            Store::Ephemeral(s) => s.get_blob(key, range).await,
            _ => Err(trc::StoreEvent::NotConfigured.into()),
        }
    }

    pub async fn put_blob(&self, key: &[u8], data: &[u8]) -> trc::Result<()> {
        match &self.primary {
            #[cfg(feature = "sqlite")]
            Store::SQLite(s) => s.put_blob(key, data).await,
            #[cfg(feature = "foundation")]
            Store::FoundationDb(s) => s.put_blob(key, data).await,
            #[cfg(feature = "postgres")]
            Store::PostgreSQL(s) => s.put_blob(key, data).await,
            #[cfg(feature = "mysql")]
            Store::MySQL(s) => s.put_blob(key, data).await,
            #[cfg(feature = "rocks")]
            Store::RocksDb(s) => s.put_blob(key, data).await,
            Store::Ephemeral(s) => s.put_blob(key, data).await,
            _ => Err(trc::StoreEvent::NotConfigured.into()),
        }
    }

    pub async fn delete_blob(&self, key: &[u8]) -> trc::Result<bool> {
        match &self.primary {
            #[cfg(feature = "sqlite")]
            Store::SQLite(s) => s.delete_blob(key).await,
            #[cfg(feature = "foundation")]
            Store::FoundationDb(s) => s.delete_blob(key).await,
            #[cfg(feature = "postgres")]
            Store::PostgreSQL(s) => s.delete_blob(key).await,
            #[cfg(feature = "mysql")]
            Store::MySQL(s) => s.delete_blob(key).await,
            #[cfg(feature = "rocks")]
            Store::RocksDb(s) => s.delete_blob(key).await,
            Store::Ephemeral(s) => s.delete_blob(key).await,
            _ => Err(trc::StoreEvent::NotConfigured.into()),
        }
    }
}
