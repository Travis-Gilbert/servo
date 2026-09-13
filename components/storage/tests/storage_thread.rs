/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use malloc_size_of::{MallocSizeOf, MallocSizeOfOps};
use profile::mem as profile_mem;
use profile::time as profile_time;
use profile_traits::generic_callback::GenericCallback as ProfiledCallback;
use servo_base::generic_channel::{self, GenericCallback, GenericSend};
use servo_base::id::{BrowsingContextId, Index, PipelineNamespaceId, TEST_WEBVIEW_ID, WebViewId};
use servo_url::ServoUrl;
use storage_traits::cache_storage::{
    CacheStorageEngine, CacheStorageEngineFactory, CacheStorageError, CacheStorageThreadMessage,
    CacheStorageThreadResponse,
};
use storage_traits::client_storage::{
    ClientStorageThreadMessage, StorageIdentifier, StorageProxyMap, StorageType,
};
use storage_traits::indexeddb::{
    BackendResult, ConnectionMsg, CreateObjectResult, IndexedDBDescription, IndexedDBIndex,
    IndexedDBThreadMsg, IndexedDbEngineFactory, KeyPath, KvsEngine, KvsTransaction, SyncOperation,
};
use storage_traits::webstorage_thread::{
    WebStorageEngine, WebStorageEngineFactory, WebStorageThreadMsg, WebStorageType,
};
use storage_traits::{StorageEngines, StorageThreads};
use uuid::Uuid;

fn shutdown_storage_group(threads: &StorageThreads) {
    let (client_sender, client_receiver) = generic_channel::channel().unwrap();
    GenericSend::send(threads, ClientStorageThreadMessage::Exit(client_sender))
        .expect("failed to send client storage exit");
    client_receiver
        .recv()
        .expect("failed to receive client storage exit ack");

    let (cache_sender, cache_receiver) = generic_channel::channel().unwrap();
    GenericSend::send(
        threads,
        CacheStorageThreadMessage::Exit(cache_sender.into()),
    )
    .expect("failed to send cache storage exit");
    cache_receiver
        .recv()
        .expect("failed to receive cache storage exit ack");

    let (idb_sender, idb_receiver) = generic_channel::channel().unwrap();
    GenericSend::send(
        threads,
        IndexedDBThreadMsg::Sync(SyncOperation::Exit(idb_sender)),
    )
    .expect("failed to send indexeddb exit");
    idb_receiver
        .recv()
        .expect("failed to receive indexeddb exit ack");

    let (web_storage_sender, web_storage_receiver) = generic_channel::channel().unwrap();
    GenericSend::send(threads, WebStorageThreadMsg::Exit(web_storage_sender))
        .expect("failed to send web storage exit");
    web_storage_receiver
        .recv()
        .expect("failed to receive web storage exit ack");
}

#[test]
fn test_new_storage_threads_create_independent_groups() {
    let mem_profiler_chan = profile_mem::Profiler::create();
    let (private_storage_threads, public_storage_threads) =
        storage::new_storage_threads(mem_profiler_chan, None, false, Default::default());

    shutdown_storage_group(&private_storage_threads);
    shutdown_storage_group(&public_storage_threads);

    // Workaround for https://github.com/servo/servo/issues/32912
    #[cfg(windows)]
    std::thread::sleep(std::time::Duration::from_millis(1000));
}

struct AlwaysPresentCache;

impl CacheStorageEngine for AlwaysPresentCache {
    fn has_cache(
        &mut self,
        origin: &servo_url::ImmutableOrigin,
        proxy: &StorageProxyMap,
        cache_name: &str,
    ) -> Result<bool, CacheStorageError<String>> {
        Ok(origin.ascii_serialization() == "https://example.com"
            && proxy.bottle_id == 42
            && cache_name == "selected")
    }
}

struct InMemoryCacheFactory;

impl CacheStorageEngineFactory for InMemoryCacheFactory {
    fn open(&self, _storage_dir: PathBuf) -> Result<Box<dyn CacheStorageEngine>, String> {
        Ok(Box::new(AlwaysPresentCache))
    }
}

#[derive(Default)]
struct InMemoryWebStorageEngine {
    values: BTreeMap<String, String>,
    clear_event: Option<(Arc<Mutex<Vec<WebStorageOpen>>>, WebStorageOpen)>,
}

impl WebStorageEngine for InMemoryWebStorageEngine {
    fn len(&self) -> Result<usize, String> {
        Ok(self.values.len())
    }

    fn key(&self, index: usize) -> Result<Option<String>, String> {
        Ok(self.values.keys().nth(index).cloned())
    }

    fn keys(&self) -> Result<Vec<String>, String> {
        Ok(self.values.keys().cloned().collect())
    }

    fn get(&self, key: &str) -> Result<Option<String>, String> {
        Ok(self.values.get(key).cloned())
    }

    fn set(&mut self, key: &str, value: &str) -> Result<Option<String>, String> {
        Ok(self.values.insert(key.to_owned(), value.to_owned()))
    }

    fn delete(&mut self, key: &str) -> Result<Option<String>, String> {
        Ok(self.values.remove(key))
    }

    fn clear(&mut self) -> Result<bool, String> {
        let changed = !self.values.is_empty();
        self.values.clear();
        if let Some((events, event)) = &self.clear_event {
            events.lock().unwrap().push(event.clone());
        }
        Ok(changed)
    }

    fn size(&self) -> Result<usize, String> {
        Ok(self
            .values
            .iter()
            .map(|(key, value)| key.len() + value.len())
            .sum())
    }
}

type WebStorageOpen = (WebStorageType, Option<WebViewId>, String);

struct RecordingWebStorageFactory {
    opens: Arc<Mutex<Vec<WebStorageOpen>>>,
    clears: Arc<Mutex<Vec<WebStorageOpen>>>,
}

impl WebStorageEngineFactory for RecordingWebStorageFactory {
    fn open(
        &self,
        storage_type: WebStorageType,
        webview_id: Option<WebViewId>,
        origin: &servo_url::ImmutableOrigin,
        _db_dir: Option<PathBuf>,
    ) -> Result<Box<dyn WebStorageEngine>, String> {
        let context = (storage_type, webview_id, origin.ascii_serialization());
        self.opens.lock().unwrap().push(context.clone());
        Ok(Box::new(InMemoryWebStorageEngine {
            values: BTreeMap::new(),
            clear_event: Some((self.clears.clone(), context)),
        }))
    }
}

fn set_web_storage(
    threads: &StorageThreads,
    storage_type: WebStorageType,
    webview_id: WebViewId,
    origin: &servo_url::ImmutableOrigin,
    value: &str,
) {
    let (sender, receiver) = generic_channel::channel().unwrap();
    GenericSend::send(
        threads,
        WebStorageThreadMsg::SetItem(
            sender,
            storage_type,
            webview_id,
            origin.clone(),
            "context".to_owned(),
            value.to_owned(),
        ),
    )
    .unwrap();
    assert_eq!(receiver.recv().unwrap(), Ok((true, None)));
}

fn get_web_storage(
    threads: &StorageThreads,
    storage_type: WebStorageType,
    webview_id: WebViewId,
    origin: &servo_url::ImmutableOrigin,
) -> Option<String> {
    let (sender, receiver) = generic_channel::channel().unwrap();
    GenericSend::send(
        threads,
        WebStorageThreadMsg::GetItem(
            sender,
            storage_type,
            webview_id,
            origin.clone(),
            "context".to_owned(),
        ),
    )
    .unwrap();
    receiver.recv().unwrap()
}

#[test]
fn test_storage_engine_factory_is_selected_end_to_end() {
    let mem_profiler_chan = profile_mem::Profiler::create();
    let config_dir = tempfile::tempdir().unwrap();
    let web_storage_opens = Arc::new(Mutex::new(Vec::new()));
    let web_storage_clears = Arc::new(Mutex::new(Vec::new()));
    let engines = StorageEngines {
        cache: Some(Arc::new(InMemoryCacheFactory)),
        web_storage: Some(Arc::new(RecordingWebStorageFactory {
            opens: web_storage_opens.clone(),
            clears: web_storage_clears.clone(),
        })),
        ..Default::default()
    };
    let (private_storage_threads, public_storage_threads) = storage::new_storage_threads(
        mem_profiler_chan,
        Some(config_dir.path().to_path_buf()),
        false,
        engines,
    );

    let (callback, receiver) = GenericCallback::new_blocking().unwrap();
    let proxy = StorageProxyMap {
        bottle_id: 42,
        handle: public_storage_threads.client_storage_handle(),
    };
    GenericSend::send(
        &public_storage_threads,
        CacheStorageThreadMessage::HasCache {
            cache_name: "selected".to_string(),
            callback,
            proxy,
            origin: ServoUrl::parse("https://example.com").unwrap().origin(),
        },
    )
    .unwrap();

    let CacheStorageThreadResponse::HasCacheResult(result) = receiver.recv().unwrap();
    assert!(result.unwrap());

    let origin = ServoUrl::parse("https://example.com").unwrap().origin();
    let other_webview = WebViewId::mock_for_testing(BrowsingContextId {
        namespace_id: PipelineNamespaceId(999),
        index: Index::new(2).unwrap(),
    });

    set_web_storage(
        &public_storage_threads,
        WebStorageType::Local,
        TEST_WEBVIEW_ID,
        &origin,
        "local",
    );
    assert_eq!(
        get_web_storage(
            &public_storage_threads,
            WebStorageType::Local,
            other_webview,
            &origin,
        ),
        Some("local".to_owned())
    );

    set_web_storage(
        &public_storage_threads,
        WebStorageType::Session,
        TEST_WEBVIEW_ID,
        &origin,
        "session-one",
    );
    set_web_storage(
        &public_storage_threads,
        WebStorageType::Session,
        other_webview,
        &origin,
        "session-two",
    );
    assert_eq!(
        get_web_storage(
            &public_storage_threads,
            WebStorageType::Session,
            TEST_WEBVIEW_ID,
            &origin,
        ),
        Some("session-one".to_owned())
    );
    assert_eq!(
        get_web_storage(
            &public_storage_threads,
            WebStorageType::Session,
            other_webview,
            &origin,
        ),
        Some("session-two".to_owned())
    );

    let opens = web_storage_opens.lock().unwrap();
    assert!(opens.contains(&(
        WebStorageType::Local,
        None,
        "https://example.com".to_owned()
    )));
    assert!(opens.contains(&(
        WebStorageType::Session,
        Some(TEST_WEBVIEW_ID),
        "https://example.com".to_owned(),
    )));
    assert!(opens.contains(&(
        WebStorageType::Session,
        Some(other_webview),
        "https://example.com".to_owned(),
    )));
    drop(opens);

    public_storage_threads.clear_webstorage_for_sites(WebStorageType::Local, &["example.com"]);
    assert!(web_storage_clears.lock().unwrap().contains(&(
        WebStorageType::Local,
        None,
        "https://example.com".to_owned(),
    )));
    assert!(
        public_storage_threads
            .webstorage_origins(WebStorageType::Local)
            .is_empty()
    );

    shutdown_storage_group(&private_storage_threads);
    shutdown_storage_group(&public_storage_threads);
}

/// A version no fresh SQLite database reports, so the assertion below cannot pass by
/// accident if the fallback engine is ever selected instead of the supplied one.
const CUSTOM_ENGINE_VERSION: u64 = 7;
const CUSTOM_ENGINE_STORE: &str = "store-only-the-custom-engine-has";

/// Names of the [`KvsEngine`] methods the manager called, in call order.
type EngineCalls = Arc<Mutex<Vec<&'static str>>>;
/// One `open`, as (database name, origin, whether client storage reported it created).
type FactoryOpen = (String, String, bool);

/// A `KvsEngine` that stores nothing and reports what it was asked.
///
/// Its answers are deliberately values SQLite could not produce on a database that has
/// just been created: a version of 7 and a store that was never created.
struct RecordingIdbEngine {
    calls: EngineCalls,
}

impl RecordingIdbEngine {
    fn record(&self, call: &'static str) {
        self.calls.lock().unwrap().push(call);
    }
}

impl MallocSizeOf for RecordingIdbEngine {
    fn size_of(&self, _ops: &mut MallocSizeOfOps) -> usize {
        0
    }
}

impl KvsEngine for RecordingIdbEngine {
    fn create_store(
        &self,
        _store_name: &str,
        _key_path: Option<KeyPath>,
        _auto_increment: bool,
    ) -> BackendResult<CreateObjectResult> {
        self.record("create_store");
        Ok(CreateObjectResult::Created)
    }

    fn delete_store(&self, _store_name: &str) -> BackendResult<()> {
        self.record("delete_store");
        Ok(())
    }

    fn close_store(&self, _store_name: &str) -> BackendResult<()> {
        self.record("close_store");
        Ok(())
    }

    fn process_transaction(
        &self,
        _transaction: KvsTransaction,
        on_complete: Box<dyn FnOnce() + Send + 'static>,
    ) {
        self.record("process_transaction");
        on_complete();
    }

    fn key_generator_current_number(&self, _store_name: &str) -> BackendResult<Option<i64>> {
        self.record("key_generator_current_number");
        Ok(None)
    }

    fn set_key_generator_current_number(
        &self,
        _store_name: &str,
        _current_number: i64,
    ) -> BackendResult<()> {
        self.record("set_key_generator_current_number");
        Ok(())
    }

    fn key_path(&self, _store_name: &str) -> BackendResult<Option<KeyPath>> {
        self.record("key_path");
        Ok(None)
    }

    fn object_store_names(&self) -> BackendResult<Vec<String>> {
        self.record("object_store_names");
        Ok(vec![CUSTOM_ENGINE_STORE.to_owned()])
    }

    fn indexes(&self, _store_name: &str) -> BackendResult<Vec<IndexedDBIndex>> {
        self.record("indexes");
        Ok(Vec::new())
    }

    fn create_index(
        &self,
        _store_name: &str,
        _index_name: String,
        _key_path: KeyPath,
        _unique: bool,
        _multi_entry: bool,
    ) -> BackendResult<CreateObjectResult> {
        self.record("create_index");
        Ok(CreateObjectResult::Created)
    }

    fn delete_index(&self, _store_name: &str, _index_name: String) -> BackendResult<()> {
        self.record("delete_index");
        Ok(())
    }

    fn rename_store(&self, _store_name: &str, _new_name: &str) -> BackendResult<()> {
        self.record("rename_store");
        Ok(())
    }

    fn rename_index(
        &self,
        _store_name: &str,
        _index_name: &str,
        _new_name: &str,
    ) -> BackendResult<()> {
        self.record("rename_index");
        Ok(())
    }

    fn version(&self) -> BackendResult<u64> {
        self.record("version");
        Ok(CUSTOM_ENGINE_VERSION)
    }

    fn set_version(&self, _version: u64) -> BackendResult<()> {
        self.record("set_version");
        Ok(())
    }

    fn rollback_transaction(&self, _serial_number: u64) -> BackendResult<()> {
        self.record("rollback_transaction");
        Ok(())
    }

    fn commit_transaction(&self, _serial_number: u64) -> BackendResult<()> {
        self.record("commit_transaction");
        Ok(())
    }
}

struct RecordingIdbFactory {
    opens: Arc<Mutex<Vec<FactoryOpen>>>,
    calls: EngineCalls,
}

impl IndexedDbEngineFactory for RecordingIdbFactory {
    fn open(
        &self,
        _path: PathBuf,
        created: bool,
        description: &IndexedDBDescription,
    ) -> BackendResult<Box<dyn KvsEngine>> {
        self.opens.lock().unwrap().push((
            description.name.clone(),
            description.origin.ascii_serialization(),
            created,
        ));
        Ok(Box::new(RecordingIdbEngine {
            calls: self.calls.clone(),
        }))
    }
}

/// An embedder-supplied IndexedDB engine is the one the manager opens and answers from.
///
/// `test_storage_engine_factory_is_selected_end_to_end` proves the `StorageEngines` seam for
/// the cache and web storage threads and leaves `indexeddb` as `None`, so until this test
/// nothing showed that a non-SQLite `KvsEngine` reaches the IndexedDB manager at all. The WPT
/// corpus cannot show it either: every WPT run takes the `SqliteIndexedDbEngineFactory`
/// fallback, which is what `None` selects.
///
/// The discriminator is in the values, not in the call log alone. Both fields asserted on the
/// connection reply are read back off the supplied engine by the manager, and neither is a
/// value SQLite could produce for a database that has just been created: SQLite would report
/// version 0 and no object stores.
#[test]
fn test_indexeddb_engine_factory_serves_the_connection() {
    let mem_profiler_chan = profile_mem::Profiler::create();
    let config_dir = tempfile::tempdir().unwrap();
    let opens: Arc<Mutex<Vec<FactoryOpen>>> = Arc::new(Mutex::new(Vec::new()));
    let calls: EngineCalls = Arc::new(Mutex::new(Vec::new()));
    let engines = StorageEngines {
        indexeddb: Some(Arc::new(RecordingIdbFactory {
            opens: opens.clone(),
            calls: calls.clone(),
        })),
        ..Default::default()
    };
    let (private_storage_threads, public_storage_threads) = storage::new_storage_threads(
        mem_profiler_chan,
        Some(config_dir.path().to_path_buf()),
        false,
        engines,
    );

    // The manager asks client storage for the database's directory before it reaches the
    // engine factory, and that lookup is a foreign key into the registry, so the bottle has to
    // be a real one rather than an invented id.
    let origin = ServoUrl::parse("https://example.com").unwrap().origin();
    let proxy = public_storage_threads
        .client_storage_handle()
        .obtain_a_storage_bottle_map(
            StorageType::Local,
            Some(TEST_WEBVIEW_ID),
            StorageIdentifier::IndexedDB,
            origin.clone(),
        )
        .recv()
        .expect("no reply to obtain_a_storage_bottle_map")
        .expect("failed to obtain a storage bottle map");
    // `SyncOperation::OpenDatabase` carries `profile_traits`' callback, which is a different
    // type from the `servo_base` one the cache storage message above uses and has no blocking
    // constructor, so the reply is handed back over a plain channel.
    let (reply_sender, receiver) = mpsc::channel();
    let callback =
        ProfiledCallback::new(profile_time::Profiler::create(&None, None), move |reply| {
            let _ = reply_sender.send(reply.expect("the open reply did not survive the channel"));
        })
        .expect("failed to build the open callback");
    GenericSend::send(
        &public_storage_threads,
        IndexedDBThreadMsg::Sync(SyncOperation::OpenDatabase(
            callback,
            origin,
            "engine-selection-db".to_owned(),
            Some(CUSTOM_ENGINE_VERSION),
            Uuid::new_v4(),
            proxy,
        )),
    )
    .expect("failed to send OpenDatabase");

    match receiver.recv().expect("no reply to OpenDatabase") {
        ConnectionMsg::Connection {
            name,
            version,
            upgraded,
            object_store_names,
            ..
        } => {
            assert_eq!(name, "engine-selection-db");
            assert_eq!(
                version, CUSTOM_ENGINE_VERSION,
                "the connection should carry the supplied engine's version, not SQLite's 0"
            );
            assert!(
                !upgraded,
                "the requested version equals the engine's, so nothing should have been upgraded"
            );
            assert_eq!(
                object_store_names,
                vec![CUSTOM_ENGINE_STORE.to_owned()],
                "the connection's scope should come from the supplied engine"
            );
        },
        other => panic!("expected a connection served by the supplied engine, got {other:?}"),
    }

    {
        let opens = opens.lock().unwrap();
        assert_eq!(
            opens.len(),
            1,
            "the manager should have opened the supplied engine exactly once, got {opens:?}"
        );
        assert_eq!(opens[0].0, "engine-selection-db");
        assert_eq!(opens[0].1, "https://example.com");
        assert!(
            opens[0].2,
            "a database that did not exist yet should reach the factory as created"
        );

        let calls = calls.lock().unwrap();
        assert!(
            calls.contains(&"version"),
            "the manager should have read the version off the supplied engine, got {calls:?}"
        );
        assert!(
            calls.contains(&"object_store_names"),
            "the manager should have read the scope off the supplied engine, got {calls:?}"
        );
    }

    shutdown_storage_group(&public_storage_threads);
    shutdown_storage_group(&private_storage_threads);
}
