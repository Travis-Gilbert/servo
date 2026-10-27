/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::path::PathBuf;

use profile_traits::mem::ProfilerChan as MemProfilerChan;
use servo_base::generic_channel::GenericSender;
use servo_base::id::StoragePartitionId;
use storage_traits::cache_storage::CacheStorageThreadHandle;
use storage_traits::client_storage::ClientStorageThreadHandle;
use storage_traits::indexeddb::IndexedDBThreadMsg;
use storage_traits::webstorage_thread::WebStorageThreadMsg;
use storage_traits::{StorageEngines, StorageThreads};

use crate::{
    CacheStorageThreadFactory, ClientStorageThreadFactory, IndexedDBThreadFactory,
    WebStorageThreadFactory,
};

pub fn new_storage_thread_group(
    mem_profiler_chan: MemProfilerChan,
    config_dir: Option<PathBuf>,
    temporary_storage: bool,
    label: &str,
    engines: &StorageEngines,
) -> StorageThreads {
    new_partition_storage_threads(
        StoragePartitionId::DEFAULT,
        mem_profiler_chan,
        if temporary_storage { None } else { config_dir },
        temporary_storage,
        label,
        engines,
    )
}

pub fn new_partition_storage_threads(
    partition_id: StoragePartitionId,
    mem_profiler_chan: MemProfilerChan,
    config_dir: Option<PathBuf>,
    ephemeral: bool,
    label: &str,
    engines: &StorageEngines,
) -> StorageThreads {
    let temporary_storage = false;
    let client_storage: ClientStorageThreadHandle = ClientStorageThreadFactory::new(
        config_dir.clone(),
        temporary_storage,
        engines.registry.clone(),
    );
    let idb: GenericSender<IndexedDBThreadMsg> = IndexedDBThreadFactory::new(
        mem_profiler_chan.clone(),
        format!("indexedDB-reporter-{label}"),
        engines.indexeddb.clone(),
    );
    let web_storage: GenericSender<WebStorageThreadMsg> = WebStorageThreadFactory::new(
        if ephemeral { None } else { config_dir.clone() },
        mem_profiler_chan,
        format!("storage-reporter-{label}"),
        engines.web_storage.clone(),
    );
    let cache_storage: CacheStorageThreadHandle =
        CacheStorageThreadFactory::new(config_dir, temporary_storage, engines.cache.clone());

    StorageThreads::for_partition(
        partition_id,
        client_storage.into(),
        idb,
        web_storage,
        cache_storage.into(),
    )
}

pub fn new_storage_threads(
    mem_profiler_chan: MemProfilerChan,
    config_dir: Option<PathBuf>,
    temporary_storage: bool,
    engines: StorageEngines,
) -> (StorageThreads, StorageThreads) {
    // Preserve the legacy pair's configured root for both groups. Explicit
    // partitions use new_partition_storage_threads directly with their own roots.
    let private_storage_threads = new_partition_storage_threads(
        StoragePartitionId::PRIVATE,
        mem_profiler_chan.clone(),
        if temporary_storage {
            None
        } else {
            config_dir.clone()
        },
        temporary_storage,
        "private",
        &engines,
    );
    let public_storage_threads = new_storage_thread_group(
        mem_profiler_chan,
        config_dir,
        temporary_storage,
        "public",
        &engines,
    );

    (private_storage_threads, public_storage_threads)
}
