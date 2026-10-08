/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::sync::mpsc;
use std::time::Duration;

use profile::{mem as profile_mem, time as profile_time};
use profile_traits::generic_callback::GenericCallback as ProfiledCallback;
use servo_base::generic_channel::{self, GenericCallback, GenericSend};
use servo_base::id::{BrowsingContextId, Index, PipelineNamespaceId, TEST_WEBVIEW_ID, WebViewId};
use servo_url::{ImmutableOrigin, ServoUrl};
use storage_traits::StorageThreads;
use storage_traits::cache_storage::{CacheStorageThreadMessage, CacheStorageThreadResponse};
use storage_traits::client_storage::{ClientStorageThreadMessage, StorageIdentifier, StorageType};
use storage_traits::indexeddb::{ConnectionMsg, IndexedDBThreadMsg, SyncOperation, TxnCompleteMsg};
use storage_traits::webstorage_thread::{WebStorageThreadMsg, WebStorageType};
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
        storage::new_storage_threads(mem_profiler_chan, None, false);

    shutdown_storage_group(&private_storage_threads);
    shutdown_storage_group(&public_storage_threads);

    // Workaround for https://github.com/servo/servo/issues/32912
    #[cfg(windows)]
    std::thread::sleep(std::time::Duration::from_millis(1000));
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

/// Default selection must keep local data durable and sessions scoped to one
/// webview in memory. Selecting a durable backend for sessions fails this test.
#[test]
fn test_default_storage_preserves_profile_and_session_boundaries() {
    let profile = tempfile::tempdir().unwrap();
    let config_dir = Some(profile.path().to_path_buf());
    let (private, public) =
        storage::new_storage_threads(profile_mem::Profiler::create(), config_dir.clone(), false);
    let origin = ServoUrl::parse("https://example.com").unwrap().origin();
    let foreign = ServoUrl::parse("https://other.example").unwrap().origin();
    let other_webview = WebViewId::mock_for_testing(BrowsingContextId {
        namespace_id: PipelineNamespaceId(999),
        index: Index::new(2).unwrap(),
    });
    set_web_storage(
        &public,
        WebStorageType::Local,
        TEST_WEBVIEW_ID,
        &origin,
        "durable",
    );
    assert_eq!(
        get_web_storage(&public, WebStorageType::Local, other_webview, &origin),
        Some("durable".into())
    );
    assert_eq!(
        get_web_storage(&public, WebStorageType::Local, TEST_WEBVIEW_ID, &foreign),
        None
    );
    set_web_storage(
        &public,
        WebStorageType::Session,
        TEST_WEBVIEW_ID,
        &origin,
        "session-one",
    );
    set_web_storage(
        &public,
        WebStorageType::Session,
        other_webview,
        &origin,
        "session-two",
    );
    assert_eq!(
        get_web_storage(&public, WebStorageType::Session, TEST_WEBVIEW_ID, &origin),
        Some("session-one".into())
    );
    assert_eq!(
        get_web_storage(&public, WebStorageType::Session, other_webview, &origin),
        Some("session-two".into())
    );
    assert_eq!(
        get_web_storage(&public, WebStorageType::Session, TEST_WEBVIEW_ID, &foreign),
        None
    );
    shutdown_storage_group(&private);
    shutdown_storage_group(&public);

    // Independent disk readback catches a volatile substitute even if a manager
    // cache answers every same-process read correctly.
    let namespace = Uuid::from_bytes([
        0x37, 0x9e, 0x56, 0xb0, 0x1a, 0x76, 0x44, 0xc5, 0xa4, 0xdb, 0xe2, 0x18, 0xc5, 0xc8, 0xa3,
        0x5d,
    ]);
    let local_dir = profile
        .path()
        .join("webstorage")
        .join(Uuid::new_v5(&namespace, b"https://example.com").to_string());
    let connection = rusqlite::Connection::open(local_dir.join("webstorage.sqlite")).unwrap();
    let value: String = connection
        .query_row("SELECT value FROM data WHERE key = ?", ["context"], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(value, "durable");
    assert!(!profile.path().join("webstorage/session").exists());
    drop(connection);

    let (private, public) =
        storage::new_storage_threads(profile_mem::Profiler::create(), config_dir, false);
    assert_eq!(
        get_web_storage(&public, WebStorageType::Local, TEST_WEBVIEW_ID, &origin),
        Some("durable".into())
    );
    assert_eq!(
        get_web_storage(&public, WebStorageType::Session, TEST_WEBVIEW_ID, &origin),
        None
    );
    assert_eq!(
        get_web_storage(&public, WebStorageType::Session, other_webview, &origin),
        None
    );
    set_web_storage(
        &public,
        WebStorageType::Local,
        TEST_WEBVIEW_ID,
        &foreign,
        "foreign-durable",
    );
    public.clear_webstorage_for_sites(WebStorageType::Local, &["example.com"]);
    // Reads already registered the foreign origin. Clearing one site must
    // remove only its descriptor and preserve the unrelated site's data.
    let descriptors = public.webstorage_origins(WebStorageType::Local);
    assert_eq!(descriptors.len(), 1);
    assert_eq!(descriptors[0].name, foreign.ascii_serialization());
    assert_eq!(
        get_web_storage(&public, WebStorageType::Local, TEST_WEBVIEW_ID, &foreign),
        Some("foreign-durable".into())
    );
    assert_eq!(
        get_web_storage(&public, WebStorageType::Local, TEST_WEBVIEW_ID, &origin),
        None
    );
    shutdown_storage_group(&private);
    shutdown_storage_group(&public);
}

/// Freeze the existing limitation explicitly: the built-in CacheStorage
/// backend is a dummy, not a working persistent cache implementation.
#[test]
fn test_default_cache_storage_reports_no_cache() {
    let (private, public) =
        storage::new_storage_threads(profile_mem::Profiler::create(), None, false);
    let origin = ServoUrl::parse("https://example.com").unwrap().origin();
    let proxy = public
        .client_storage_handle()
        .obtain_a_storage_bottle_map(
            StorageType::Local,
            Some(TEST_WEBVIEW_ID),
            StorageIdentifier::Caches,
            origin.clone(),
        )
        .recv()
        .unwrap()
        .unwrap();
    let (callback, receiver) = GenericCallback::new_blocking().unwrap();
    GenericSend::send(
        &public,
        CacheStorageThreadMessage::HasCache {
            cache_name: "absent".into(),
            callback,
            proxy,
            origin,
        },
    )
    .unwrap();
    let CacheStorageThreadResponse::HasCacheResult(result) = receiver.recv().unwrap();
    assert!(!result.unwrap());
    shutdown_storage_group(&private);
    shutdown_storage_group(&public);
}

fn open_database(
    threads: &StorageThreads,
    origin: &ImmutableOrigin,
) -> (mpsc::Receiver<ConnectionMsg>, std::path::PathBuf) {
    let registry = threads.client_storage_handle();
    let proxy = registry
        .obtain_a_storage_bottle_map(
            StorageType::Local,
            Some(TEST_WEBVIEW_ID),
            StorageIdentifier::IndexedDB,
            origin.clone(),
        )
        .recv()
        .unwrap()
        .unwrap();
    let (path, _) = registry
        .create_database(proxy.bottle_id, "default-engine-db".into())
        .recv()
        .unwrap()
        .unwrap();
    let (sender, receiver) = mpsc::channel();
    let callback =
        ProfiledCallback::new(profile_time::Profiler::create(&None, None), move |reply| {
            sender
                .send(reply.expect("IndexedDB callback decoding failed"))
                .unwrap();
        })
        .unwrap();
    GenericSend::send(
        threads,
        IndexedDBThreadMsg::Sync(SyncOperation::OpenDatabase(
            callback,
            origin.clone(),
            "default-engine-db".into(),
            Some(3),
            Uuid::new_v4(),
            proxy,
        )),
    )
    .unwrap();
    (receiver, path)
}

/// Exercise the actual default constructor through registry -> manager ->
/// SQLite -> connection reply, then read disk and reopen the same profile.
/// A fresh in-memory engine or a wrong registry path cannot satisfy the reopen.
#[test]
fn test_default_indexeddb_reopens_the_registry_sqlite_database() {
    let profile = tempfile::tempdir().unwrap();
    let config_dir = Some(profile.path().to_path_buf());
    let origin = ServoUrl::parse("https://example.com").unwrap().origin();
    let (private, public) =
        storage::new_storage_threads(profile_mem::Profiler::create(), config_dir.clone(), false);
    let (receiver, path) = open_database(&public, &origin);
    let transaction = match receiver
        .recv_timeout(Duration::from_secs(10))
        .expect("no IndexedDB open reply")
    {
        ConnectionMsg::Upgrade {
            name,
            version,
            old_version,
            transaction,
            object_store_names,
            ..
        } => {
            assert_eq!(name, "default-engine-db");
            assert_eq!(version, 3);
            assert_eq!(old_version, 0);
            assert!(object_store_names.is_empty());
            transaction
        },
        other => panic!("fresh default database must request an upgrade: {other:?}"),
    };
    let (sender, version_reply) = generic_channel::channel().unwrap();
    GenericSend::send(
        &public,
        IndexedDBThreadMsg::Sync(SyncOperation::UpgradeVersion(
            sender,
            origin.clone(),
            "default-engine-db".into(),
            transaction,
            3,
        )),
    )
    .unwrap();
    assert_eq!(version_reply.recv().unwrap().unwrap(), 3);
    let (sender, committed) = mpsc::channel();
    let callback =
        ProfiledCallback::new(profile_time::Profiler::create(&None, None), move |reply| {
            sender
                .send(reply.expect("commit callback decoding failed"))
                .unwrap();
        })
        .unwrap();
    GenericSend::send(
        &public,
        IndexedDBThreadMsg::Sync(SyncOperation::Commit(
            callback,
            origin.clone(),
            "default-engine-db".into(),
            transaction,
        )),
    )
    .unwrap();
    let completion: TxnCompleteMsg = committed
        .recv_timeout(Duration::from_secs(10))
        .expect("no commit reply");
    assert!(completion.result.is_ok());
    GenericSend::send(
        &public,
        IndexedDBThreadMsg::Sync(SyncOperation::UpgradeTransactionFinished {
            origin: origin.clone(),
            db_name: "default-engine-db".into(),
            txn: transaction,
            committed: true,
        }),
    )
    .unwrap();
    assert!(matches!(
        receiver.recv_timeout(Duration::from_secs(10)).unwrap(),
        ConnectionMsg::Connection {
            version: 3,
            upgraded: true,
            ..
        }
    ));
    shutdown_storage_group(&private);
    shutdown_storage_group(&public);

    let connection = rusqlite::Connection::open(path.join("indexeddb.sqlite")).unwrap();
    let row: (String, String, u64) = connection
        .query_row("SELECT name, origin, version FROM database", [], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })
        .unwrap();
    assert_eq!(
        row,
        ("default-engine-db".into(), "https://example.com".into(), 3)
    );
    assert!(connection.table_exists(None, "object_store_index").unwrap());
    drop(connection);
    let (private, public) =
        storage::new_storage_threads(profile_mem::Profiler::create(), config_dir, false);
    let (receiver, reopened_path) = open_database(&public, &origin);
    assert_eq!(path, reopened_path);
    match receiver
        .recv_timeout(Duration::from_secs(10))
        .expect("no reopen reply")
    {
        ConnectionMsg::Connection {
            version,
            upgraded,
            object_store_names,
            ..
        } => {
            assert_eq!(version, 3);
            assert!(!upgraded);
            assert!(object_store_names.is_empty());
        },
        other => panic!("expected a durable default connection: {other:?}"),
    }
    shutdown_storage_group(&private);
    shutdown_storage_group(&public);
}
