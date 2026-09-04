/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::path::PathBuf;
use std::sync::Arc;
use std::thread;

use log::error;
use servo_base::generic_channel::{self, GenericReceiver, GenericSender};
use storage_traits::cache_storage::{
    CacheStorageEngine, CacheStorageEngineFactory, CacheStorageError, CacheStorageThreadHandle,
    CacheStorageThreadMessage, CacheStorageThreadResponse,
};

pub struct DummyCacheStorageEngine;

impl CacheStorageEngine for DummyCacheStorageEngine {
    /// <https://w3c.github.io/ServiceWorker/#cache-storage-has>
    /// The parallel steps.
    fn has_cache(
        &mut self,
        _origin: &servo_url::ImmutableOrigin,
        _proxy: &storage_traits::client_storage::StorageProxyMap,
        _cache_name: &str,
    ) -> Result<bool, CacheStorageError<String>> {
        // TODO: implement.
        // Step 2.1:For each key → value of the relevant name to cache map:
        // Step 2.1.1: If cacheName matches key, resolve promise with true and abort these steps.
        // Step 2.2: Resolve promise with false.
        // Note: promise resolved in the callback in CacheStorage.
        Ok(false)
    }
}

pub(crate) struct DefaultCacheStorageEngineFactory;

impl CacheStorageEngineFactory for DefaultCacheStorageEngineFactory {
    fn open(&self, _storage_dir: PathBuf) -> Result<Box<dyn CacheStorageEngine>, String> {
        Ok(Box::new(DummyCacheStorageEngine))
    }
}

pub trait CacheStorageThreadFactory {
    fn new(
        config_dir: Option<PathBuf>,
        temporary_storage: bool,
        factory: Option<Arc<dyn CacheStorageEngineFactory>>,
    ) -> Self;
}

impl CacheStorageThreadFactory for CacheStorageThreadHandle {
    fn new(
        config_dir: Option<PathBuf>,
        temporary_storage: bool,
        factory: Option<Arc<dyn CacheStorageEngineFactory>>,
    ) -> CacheStorageThreadHandle {
        let (generic_sender, generic_receiver) = generic_channel::channel().unwrap();
        let mut temp_dir: Option<tempfile::TempDir> = None;
        let base_dir = config_dir
            .unwrap_or_else(|| {
                let tmp_dir = tempfile::tempdir().unwrap();
                let path = tmp_dir.path().to_path_buf();
                temp_dir = Some(tmp_dir);
                path
            })
            .join("cachestorage");
        let storage_dir = if temporary_storage {
            let unique_id = uuid::Uuid::new_v4().to_string();
            base_dir.join("temporary").join(unique_id)
        } else {
            base_dir.join("default_v1")
        };
        std::fs::create_dir_all(&storage_dir)
            .expect("Failed to create CacheStorage storage directory");
        let sender_clone = generic_sender.clone();
        thread::Builder::new()
            .name("CacheStorageThread".to_owned())
            .spawn(move || {
                // Keep temp_dir alive while the thread runs.
                let _ = temp_dir;
                let factory = factory.unwrap_or_else(|| Arc::new(DefaultCacheStorageEngineFactory));
                let Ok(engine) = factory.open(storage_dir) else {
                    error!("Failed to initialize CacheStorage engine");
                    return;
                };
                CacheStorageThread::new(sender_clone, generic_receiver, engine).start();
            })
            .expect("Thread spawning failed");

        CacheStorageThreadHandle::new(generic_sender)
    }
}

struct CacheStorageThread {
    receiver: GenericReceiver<CacheStorageThreadMessage>,
    // Note: a sender to self might be required later for the storage engine.
    _sender: GenericSender<CacheStorageThreadMessage>,
    engine: Box<dyn CacheStorageEngine>,
}

impl CacheStorageThread {
    pub fn new(
        _sender: GenericSender<CacheStorageThreadMessage>,
        receiver: GenericReceiver<CacheStorageThreadMessage>,
        engine: Box<dyn CacheStorageEngine>,
    ) -> CacheStorageThread {
        CacheStorageThread {
            _sender,
            receiver,
            engine,
        }
    }

    pub fn start(&mut self) {
        while let Ok(message) = self.receiver.recv() {
            match message {
                CacheStorageThreadMessage::HasCache {
                    cache_name,
                    callback,
                    proxy,
                    origin,
                } => {
                    let result = self.engine.has_cache(&origin, &proxy, &cache_name);
                    if callback
                        .send(CacheStorageThreadResponse::HasCacheResult(
                            result.map_err(|e| format!("{:?}", e)),
                        ))
                        .is_err()
                    {
                        error!("Failed to send response to script for HasCache message.");
                    }
                },
                CacheStorageThreadMessage::Exit(sender) => {
                    let _ = sender.send(());
                    break;
                },
            }
        }
    }
}
