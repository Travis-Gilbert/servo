/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::ops::{Deref, DerefMut};
use std::path::PathBuf;

use malloc_size_of_derive::MallocSizeOf;
use serde::{Deserialize, Serialize};
use servo_base::generic_channel::{GenericCallback, GenericSender};
use servo_url::ImmutableOrigin;

use crate::client_storage::StorageProxyMap;

#[derive(Debug, Deserialize, Serialize)]
pub enum CacheStorageError<T> {
    Internal(T),
}

/// Cache Storage operations exposed to embedders that provide a storage backend.
///
/// This contract mirrors the four operations handled by Servo's CacheStorage
/// thread so an embedder backend remains authoritative for the complete API.
pub trait CacheStorageEngine: Send {
    /// <https://w3c.github.io/ServiceWorker/#cache-storage-has>
    fn has_cache(
        &mut self,
        origin: &ImmutableOrigin,
        proxy_map: &StorageProxyMap,
        cache_name: &str,
    ) -> Result<bool, CacheStorageError<String>>;

    /// <https://w3c.github.io/ServiceWorker/#cache-storage-open>
    fn open_cache(
        &mut self,
        origin: &ImmutableOrigin,
        cache_name: String,
        proxy_map: &StorageProxyMap,
    ) -> Result<(), CacheStorageError<String>>;

    /// <https://w3c.github.io/ServiceWorker/#cache-keys>
    fn keys(
        &mut self,
        origin: &ImmutableOrigin,
        cache_name: &str,
    ) -> Result<Vec<String>, CacheStorageError<String>>;

    /// <https://w3c.github.io/ServiceWorker/#dom-cachestorage-delete>
    fn delete_cache(
        &mut self,
        origin: &ImmutableOrigin,
        cache_name: &str,
        proxy_map: &StorageProxyMap,
    ) -> Result<bool, CacheStorageError<String>>;
}

/// Creates a Cache Storage backend for one storage thread.
pub trait CacheStorageEngineFactory: Send + Sync {
    fn open(&self, storage_dir: PathBuf) -> Result<Box<dyn CacheStorageEngine>, String>;
}

#[derive(Clone, Debug, Deserialize, MallocSizeOf, Serialize)]
pub struct CacheStorageThreadHandle {
    sender: GenericSender<CacheStorageThreadMessage>,
}

impl CacheStorageThreadHandle {
    pub fn new(sender: GenericSender<CacheStorageThreadMessage>) -> Self {
        CacheStorageThreadHandle { sender }
    }
}

impl From<CacheStorageThreadHandle> for GenericSender<CacheStorageThreadMessage> {
    fn from(handle: CacheStorageThreadHandle) -> Self {
        handle.sender
    }
}

impl From<GenericSender<CacheStorageThreadMessage>> for CacheStorageThreadHandle {
    fn from(sender: GenericSender<CacheStorageThreadMessage>) -> Self {
        CacheStorageThreadHandle::new(sender)
    }
}

impl Deref for CacheStorageThreadHandle {
    type Target = GenericSender<CacheStorageThreadMessage>;

    fn deref(&self) -> &Self::Target {
        &self.sender
    }
}

impl DerefMut for CacheStorageThreadHandle {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.sender
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum CacheStorageThreadMessage {
    /// <https://w3c.github.io/ServiceWorker/#cache-storage-has>
    HasCache {
        cache_name: String,
        callback: GenericCallback<CacheStorageThreadResponse>,
        proxy: StorageProxyMap,
        origin: ImmutableOrigin,
    },
    /// <https://w3c.github.io/ServiceWorker/#cache-storage-open>
    OpenCache {
        cache_name: String,
        callback: GenericCallback<CacheStorageThreadResponse>,
        proxy: StorageProxyMap,
        origin: ImmutableOrigin,
    },
    /// <https://w3c.github.io/ServiceWorker/#cache-keys>
    Keys {
        cache_name: String,
        callback: GenericCallback<CacheStorageThreadResponse>,
        origin: ImmutableOrigin,
    },
    DeleteCache {
        cache_name: String,
        callback: GenericCallback<CacheStorageThreadResponse>,
        proxy: StorageProxyMap,
        origin: ImmutableOrigin,
    },
    Exit(GenericSender<()>),
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum CacheStorageThreadResponse {
    HasCacheResult(Result<bool, String>),
    OpenCacheResult {
        result: Result<(), String>,
        cache_name: String,
    },
    KeysResult(Result<Vec<String>, String>),
    DeleteCacheResult(Result<bool, String>),
}
