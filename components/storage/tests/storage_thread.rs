/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use profile::mem as profile_mem;
use servo_base::generic_channel::{self, GenericCallback, GenericSend};
use servo_base::id::{BrowsingContextId, Index, PipelineNamespaceId, TEST_WEBVIEW_ID, WebViewId};
use servo_url::ServoUrl;
use storage_traits::cache_storage::{
    CacheStorageEngine, CacheStorageEngineFactory, CacheStorageError, CacheStorageThreadMessage,
    CacheStorageThreadResponse,
};
use storage_traits::client_storage::{ClientStorageThreadMessage, StorageProxyMap};
use storage_traits::indexeddb::{IndexedDBThreadMsg, SyncOperation};
use storage_traits::webstorage_thread::{
    WebStorageEngine, WebStorageEngineFactory, WebStorageThreadMsg, WebStorageType,
};
use storage_traits::{StorageEngines, StorageThreads};

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
