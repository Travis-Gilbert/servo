/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::net::IpAddr;

use net::connector::CACertificates;
use net::protocols::ProtocolRegistry;
use net::resource_thread::new_core_resource_thread;
use net::test::parse_hostsfile;
use net_traits::CoreResourceMsg;
use profile_traits::mem::ProfilerChan as MemProfilerChan;
use profile_traits::time::ProfilerChan;
use servo_base::generic_channel;

use crate::create_generic_embedder_proxy;

fn ip(s: &str) -> IpAddr {
    s.parse().unwrap()
}

#[test]
fn test_exit() {
    let (tx, _rx) = generic_channel::channel().unwrap();
    let (mtx, _mrx) = generic_channel::channel().unwrap();
    let (sender, receiver) = generic_channel::oneshot().unwrap();
    let (resource_thread, private_resource_thread) = new_core_resource_thread(
        None,
        ProfilerChan(Some(tx)),
        MemProfilerChan(mtx),
        create_generic_embedder_proxy(),
        None,
        CACertificates::Default,
        false, /* ignore_certificate_errors */
        std::sync::Arc::new(ProtocolRegistry::default()),
    );
    resource_thread.send(CoreResourceMsg::Exit(sender)).unwrap();
    receiver.recv().unwrap();
    let (sender, receiver) = generic_channel::oneshot().unwrap();
    private_resource_thread
        .send(CoreResourceMsg::Exit(sender))
        .unwrap();
    receiver.recv().unwrap();
    // Workaround for https://github.com/servo/servo/issues/32912
    #[cfg(windows)]
    std::thread::sleep(std::time::Duration::from_millis(100));
}

#[test]
fn test_parse_hostsfile() {
    let mock_hosts_file_content = "127.0.0.1 foo.bar.com\n127.0.0.2 servo.test.server";
    let hosts_table = parse_hostsfile(mock_hosts_file_content);
    assert_eq!(2, hosts_table.len());
    assert_eq!(ip("127.0.0.1"), *hosts_table.get("foo.bar.com").unwrap());
    assert_eq!(
        ip("127.0.0.2"),
        *hosts_table.get("servo.test.server").unwrap()
    );
}

#[test]
fn test_parse_malformed_hostsfile() {
    let mock_hosts_file_content =
        "malformed file\n127.0.0.1 foo.bar.com\nservo.test.server 127.0.0.1";
    let hosts_table = parse_hostsfile(mock_hosts_file_content);
    assert_eq!(1, hosts_table.len());
    assert_eq!(ip("127.0.0.1"), *hosts_table.get("foo.bar.com").unwrap());
}

#[test]
fn test_parse_hostsfile_with_line_comment() {
    let mock_hosts_file_content =
        "# this is a line comment\n127.0.0.1 foo.bar.com\n# anothercomment";
    let hosts_table = parse_hostsfile(mock_hosts_file_content);
    assert_eq!(1, hosts_table.len());
    assert_eq!(ip("127.0.0.1"), *hosts_table.get("foo.bar.com").unwrap());
}

#[test]
fn test_parse_hostsfile_with_end_of_line_comment() {
    let mock_hosts_file_content =
        "127.0.0.1 foo.bar.com # line ending comment\n127.0.0.2 servo.test.server #comment";
    let hosts_table = parse_hostsfile(mock_hosts_file_content);
    assert_eq!(2, hosts_table.len());
    assert_eq!(ip("127.0.0.1"), *hosts_table.get("foo.bar.com").unwrap());
    assert_eq!(
        ip("127.0.0.2"),
        *hosts_table.get("servo.test.server").unwrap()
    );
}

#[test]
fn test_parse_hostsfile_with_2_hostnames_for_1_address() {
    let mock_hosts_file_content = "127.0.0.1 foo.bar.com baz.bar.com";
    let hosts_table = parse_hostsfile(mock_hosts_file_content);
    assert_eq!(2, hosts_table.len());
    assert_eq!(ip("127.0.0.1"), *hosts_table.get("foo.bar.com").unwrap());
    assert_eq!(ip("127.0.0.1"), *hosts_table.get("baz.bar.com").unwrap());
}

#[test]
fn test_parse_hostsfile_with_4_hostnames_for_1_address() {
    let mock_hosts_file_content = "127.0.0.1 moz.foo.com moz.bar.com moz.baz.com moz.moz.com";
    let hosts_table = parse_hostsfile(mock_hosts_file_content);
    assert_eq!(4, hosts_table.len());
    assert_eq!(ip("127.0.0.1"), *hosts_table.get("moz.foo.com").unwrap());
    assert_eq!(ip("127.0.0.1"), *hosts_table.get("moz.bar.com").unwrap());
    assert_eq!(ip("127.0.0.1"), *hosts_table.get("moz.baz.com").unwrap());
    assert_eq!(ip("127.0.0.1"), *hosts_table.get("moz.moz.com").unwrap());
}

#[test]
fn test_parse_hostsfile_with_tabs_instead_spaces() {
    let mock_hosts_file_content = "127.0.0.1\tfoo.bar.com\n127.0.0.2\tservo.test.server";
    let hosts_table = parse_hostsfile(mock_hosts_file_content);
    assert_eq!(2, hosts_table.len());
    assert_eq!(ip("127.0.0.1"), *hosts_table.get("foo.bar.com").unwrap());
    assert_eq!(
        ip("127.0.0.2"),
        *hosts_table.get("servo.test.server").unwrap()
    );
}

#[test]
fn test_parse_hostsfile_with_valid_ipv4_addresses() {
    let mock_hosts_file_content =
        "255.255.255.255 foo.bar.com\n169.0.1.201 servo.test.server\n192.168.5.0 servo.foo.com";
    let hosts_table = parse_hostsfile(mock_hosts_file_content);
    assert_eq!(3, hosts_table.len());
}

#[test]
fn test_parse_hostsfile_with_invalid_ipv4_addresses() {
    let mock_hosts_file_content = "256.255.255.255 foo.bar.com\n169.0.1000.201 servo.test.server \
                                   \n192.168.5.500 servo.foo.com\n192.abc.100.2 test.servo.com";
    let hosts_table = parse_hostsfile(mock_hosts_file_content);
    assert_eq!(0, hosts_table.len());
}

#[test]
fn test_parse_hostsfile_with_valid_ipv6_addresses() {
    let mock_hosts_file_content = "2001:0db8:0000:0000:0000:ff00:0042:8329 foo.bar.com\n\
                                   2001:db8:0:0:0:ff00:42:8329 moz.foo.com\n\
                                   2001:db8::ff00:42:8329 foo.moz.com moz.moz.com\n\
                                   0000:0000:0000:0000:0000:0000:0000:0001 bar.moz.com\n\
                                   ::1 foo.bar.baz baz.foo.com\n\
                                   2001:0DB8:85A3:0042:1000:8A2E:0370:7334 baz.bar.moz\n\
                                   :: unspecified.moz.com";
    let hosts_table = parse_hostsfile(mock_hosts_file_content);
    assert_eq!(9, hosts_table.len());
}

#[test]
fn test_parse_hostsfile_with_invalid_ipv6_addresses() {
    let mock_hosts_file_content = "12001:0db8:0000:0000:0000:ff00:0042:8329 foo.bar.com\n\
                                   2001:zdb8:0:0:0:gg00:42:t329 moz.foo.com\n\
                                   2002:0DB8:85A3:0042:1000:8A2E:0370:7334/1289 baz3.bar.moz";
    let hosts_table = parse_hostsfile(mock_hosts_file_content);
    assert_eq!(0, hosts_table.len());
}

#[test]
fn test_parse_hostsfile_with_end_of_line_whitespace() {
    let mock_hosts_file_content = "127.0.0.1 foo.bar.com \n\
                                   2001:db8:0:0:0:ff00:42:8329 moz.foo.com\n \
                                   127.0.0.2 servo.test.server ";
    let hosts_table = parse_hostsfile(mock_hosts_file_content);
    assert_eq!(3, hosts_table.len());
    assert_eq!(ip("127.0.0.1"), *hosts_table.get("foo.bar.com").unwrap());
    assert_eq!(
        ip("2001:db8:0:0:0:ff00:42:8329"),
        *hosts_table.get("moz.foo.com").unwrap()
    );
    assert_eq!(
        ip("127.0.0.2"),
        *hosts_table.get("servo.test.server").unwrap()
    );
}

fn partition_resource_owner(
    root: Option<std::path::PathBuf>,
    disk_path: Option<std::path::PathBuf>,
) -> net_traits::ResourceThreads {
    let (tx, _rx) = generic_channel::channel().unwrap();
    let (mtx, _mrx) = generic_channel::channel().unwrap();
    net::resource_thread::new_partition_resource_threads(
        servo_base::id::StoragePartitionId::new(),
        None,
        ProfilerChan(Some(tx)),
        MemProfilerChan(mtx),
        create_generic_embedder_proxy(),
        root,
        disk_path,
        CACertificates::Default,
        false,
        std::sync::Arc::new(ProtocolRegistry::with_internal_protocols()),
    )
}

fn stop_partition(resources: &net_traits::ResourceThreads) {
    let (sender, receiver) = generic_channel::oneshot().unwrap();
    resources
        .core_thread
        .send(CoreResourceMsg::Exit(sender))
        .unwrap();
    receiver.recv().unwrap();
}

fn partition_request(url: servo_url::ServoUrl) -> net_traits::request::RequestBuilder {
    use net_traits::request::{CredentialsMode, Referrer, RequestBuilder};
    RequestBuilder::new(
        None,
        net_traits::blob_url_store::UrlWithBlobClaim::new(url.clone(), None),
        Referrer::NoReferrer,
    )
    .origin(url.origin())
    .credentials_mode(CredentialsMode::Include)
}

fn partition_fetch(resources: &net_traits::ResourceThreads, url: servo_url::ServoUrl) -> String {
    partition_fetch_request(resources, partition_request(url))
}

fn partition_fetch_request(
    resources: &net_traits::ResourceThreads,
    request: net_traits::request::RequestBuilder,
) -> String {
    use net_traits::{FetchChannels, FetchResponseMsg};
    let (sender, receiver) = std::sync::mpsc::channel();
    let callback = generic_channel::GenericCallback::new(move |reply| {
        let _ = sender.send(reply);
    })
    .unwrap();
    resources
        .core_thread
        .send(CoreResourceMsg::Fetch(
            request,
            FetchChannels::ResponseMsg(callback),
        ))
        .unwrap();
    let mut body = Vec::new();
    loop {
        match receiver
            .recv_timeout(std::time::Duration::from_secs(30))
            .unwrap()
            .unwrap()
        {
            FetchResponseMsg::ProcessResponse(_, result) => {
                result.unwrap();
            },
            FetchResponseMsg::ProcessResponseChunk(_, chunk) => body.extend_from_slice(&chunk),
            FetchResponseMsg::ProcessResponseEOF(_, result, _) => {
                result.unwrap();
                break;
            },
            _ => {},
        }
    }
    String::from_utf8(body).unwrap()
}

#[test]
fn test_partition_cookies_http_cache_disk_roots_and_reopen() {
    use net::test_util::{make_body, make_server};
    use net_traits::CookieSource;
    use servo_config::prefs::Preferences;
    servo_config::prefs::set(Preferences {
        network_http_cache_size: 4,
        ..Default::default()
    });
    let validators = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let observed = validators.clone();
    let (server, url) = make_server(move |request, response| {
        if request.uri().path() == "/redirect" {
            *response.status_mut() = http::StatusCode::FOUND;
            response
                .headers_mut()
                .insert(http::header::LOCATION, http::HeaderValue::from_static("/"));
            return;
        }
        let cookie = request
            .headers()
            .get(http::header::COOKIE)
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        response.headers_mut().insert(
            http::header::CACHE_CONTROL,
            http::HeaderValue::from_static("public, max-age=3600"),
        );
        response.headers_mut().insert(
            http::header::ETAG,
            http::HeaderValue::from_str(&format!("\"{cookie}\"")).unwrap(),
        );
        if request.uri().path() == "/stale" {
            response.headers_mut().insert(
                http::header::CACHE_CONTROL,
                http::HeaderValue::from_static("no-cache"),
            );
            if let Some(validator) = request.headers().get(http::header::IF_NONE_MATCH) {
                observed
                    .lock()
                    .unwrap()
                    .push(validator.to_str().unwrap().to_owned());
                response
                    .headers_mut()
                    .insert(http::header::ETAG, validator.clone());
                *response.status_mut() = http::StatusCode::NOT_MODIFIED;
                return;
            }
        }
        // Deliberately omit Vary: Cookie: isolation must come from the owner.
        *response.body_mut() = make_body(cookie.into_bytes());
    });
    let url = servo_url::ServoUrl::parse(url.as_str()).unwrap();
    let stale_url = url.join("/stale").unwrap();
    let roots = [tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap()];
    let owners = roots.each_ref().map(|root| {
        partition_resource_owner(
            Some(root.path().to_owned()),
            Some(root.path().join("cache.sqlite3")),
        )
    });
    for (index, owner) in owners.iter().enumerate() {
        owner.set_cookie_for_url_sync(
            url.clone(),
            cookie::Cookie::parse(format!("profile={index}; Max-Age=3600; Path=/"))
                .unwrap()
                .into_owned(),
            CookieSource::HTTP,
        );
        assert_eq!(
            partition_fetch(owner, url.clone()),
            format!("profile={index}")
        );
        assert_eq!(
            partition_fetch(owner, stale_url.clone()),
            format!("profile={index}")
        );
    }
    for (index, owner) in owners.iter().enumerate() {
        assert_eq!(
            partition_fetch(owner, url.join("/redirect").unwrap()),
            format!("profile={index}")
        );
    }
    // Same-partition channel clones share the cached representation even if the jar changes.
    for (index, owner) in owners.iter().enumerate() {
        owner.set_cookie_for_url_sync(
            url.clone(),
            cookie::Cookie::parse("profile=changed; Max-Age=3600; Path=/")
                .unwrap()
                .into_owned(),
            CookieSource::HTTP,
        );
        assert_eq!(
            partition_fetch(&owner.clone(), url.clone()),
            format!("profile={index}")
        );
        for n in 0..32 {
            partition_fetch(owner, url.join(&format!("/evict/{n}")).unwrap());
        }
        let db = rusqlite::Connection::open(roots[index].path().join("cache.sqlite3")).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            let count: i64 = db
                .query_row(
                    "SELECT count(*) FROM disk_cache WHERE key IN (?1, ?2)",
                    [url.as_str(), stale_url.as_str()],
                    |row| row.get(0),
                )
                .unwrap();
            if count == 2 {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "cache entry was not evicted to partition disk cache"
            );
            std::thread::yield_now();
        }
        stop_partition(owner);
    }
    for (index, root) in roots.iter().enumerate() {
        let owner = partition_resource_owner(
            Some(root.path().to_owned()),
            Some(root.path().join("cache.sqlite3")),
        );
        assert_eq!(
            owner.cookies_for_url(url.clone(), CookieSource::HTTP)[0].value(),
            "changed"
        );
        assert_eq!(
            partition_fetch(&owner, url.clone()),
            format!("profile={index}")
        );
        assert_eq!(
            partition_fetch(&owner, stale_url.clone()),
            format!("profile={index}")
        );
        stop_partition(&owner);
    }
    let mut validators = validators.lock().unwrap().clone();
    validators.sort();
    assert_eq!(validators, ["\"profile=0\"", "\"profile=1\""]);
    server.close();
}

#[test]
fn test_ephemeral_network_owner_does_not_touch_configured_disk_path() {
    let root = tempfile::tempdir().unwrap();
    let disk = root.path().join("must-not-exist.sqlite3");
    servo_config::prefs::set(servo_config::prefs::Preferences {
        network_http_disk_cache: disk.to_string_lossy().into_owned(),
        ..Default::default()
    });
    let owner = partition_resource_owner(None, None);
    let url = servo_url::ServoUrl::parse("https://example.com/").unwrap();
    owner.set_cookie_for_url_sync(
        url.clone(),
        cookie::Cookie::new("ephemeral", "value"),
        net_traits::CookieSource::HTTP,
    );
    assert_eq!(
        owner
            .cookies_for_url(url, net_traits::CookieSource::HTTP)
            .len(),
        1
    );
    stop_partition(&owner);
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
}

#[test]
fn test_partition_http_auth_and_learned_hsts_reopen() {
    use headers::authorization::Basic;
    use headers::{Authorization, HeaderMapExt};
    use net::hsts::{HstsEntry, HstsList};
    use net::resource_thread::{AuthCache, AuthCacheEntry};
    use net::test_util::{make_body, make_server};
    let (server, url) = make_server(|request, response| {
        let auth = request
            .headers()
            .typed_get::<Authorization<Basic>>()
            .unwrap();
        *response.body_mut() = make_body(auth.username().as_bytes().to_vec());
        response.headers_mut().insert(
            http::header::CACHE_CONTROL,
            http::HeaderValue::from_static("no-store"),
        );
    });
    let url = servo_url::ServoUrl::parse(url.as_str()).unwrap();
    let roots = [tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap()];
    for (index, root) in roots.iter().enumerate() {
        let mut auth = AuthCache::default();
        auth.entries.insert(
            url.origin().ascii_serialization().into_owned(),
            AuthCacheEntry {
                user_name: format!("profile-{index}"),
                password: "test".into(),
            },
        );
        servo_base::write_json_to_file(&auth, root.path(), "auth_cache.json");
        let mut hsts = HstsList::default();
        hsts.push(
            HstsEntry::new(
                format!("profile-{index}.example.com"),
                net_traits::IncludeSubdomains::Included,
                Some(std::time::Duration::from_secs(3600)),
            )
            .unwrap(),
        );
        servo_base::write_json_to_file(&hsts, root.path(), "hsts_list.json");
    }
    let owners = roots
        .each_ref()
        .map(|root| partition_resource_owner(Some(root.path().to_owned()), None));
    for (index, owner) in owners.iter().enumerate() {
        assert_eq!(
            partition_fetch(owner, url.clone()),
            format!("profile-{index}")
        );
        stop_partition(owner);
    }
    for (index, root) in roots.iter().enumerate() {
        let mut hsts = HstsList::default();
        servo_base::read_json_from_file(&mut hsts, root.path(), "hsts_list.json");
        let hosts: Vec<_> = hsts
            .entries_map
            .values()
            .flatten()
            .map(|entry| entry.host.as_str())
            .collect();
        assert_eq!(hosts, vec![format!("profile-{index}.example.com")]);
        let owner = partition_resource_owner(Some(root.path().to_owned()), None);
        assert_eq!(
            partition_fetch(&owner, url.clone()),
            format!("profile-{index}")
        );
        stop_partition(&owner);
    }
    server.close();
}

#[test]
fn test_partition_cache_validators_and_concurrent_requests() {
    use net::test_util::{make_body, make_server};
    let validations = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let observed = validations.clone();
    let (server, url) = make_server(move |request, response| {
        let cookie = request.headers()[http::header::COOKIE].to_str().unwrap();
        let tag = format!("\"{cookie}\"");
        response
            .headers_mut()
            .insert(http::header::ETAG, tag.parse().unwrap());
        response.headers_mut().insert(
            http::header::CACHE_CONTROL,
            http::HeaderValue::from_static("no-cache"),
        );
        if let Some(validator) = request.headers().get(http::header::IF_NONE_MATCH) {
            assert_eq!(validator.to_str().unwrap(), tag);
            observed.lock().unwrap().push(cookie.to_owned());
            *response.status_mut() = http::StatusCode::NOT_MODIFIED;
        } else {
            *response.body_mut() = make_body(cookie.as_bytes().to_vec());
        }
    });
    let url = servo_url::ServoUrl::parse(url.as_str()).unwrap();
    let owners = [
        partition_resource_owner(None, None),
        partition_resource_owner(None, None),
    ];
    for (index, owner) in owners.iter().enumerate() {
        owner.set_cookie_for_url_sync(
            url.clone(),
            cookie::Cookie::new("profile", index.to_string()),
            net_traits::CookieSource::HTTP,
        );
    }
    for _ in 0..2 {
        std::thread::scope(|scope| {
            let jobs: Vec<_> = owners
                .iter()
                .enumerate()
                .map(|(index, owner)| {
                    let owner = owner.clone();
                    let url = url.clone();
                    scope.spawn(move || {
                        assert_eq!(partition_fetch(&owner, url), format!("profile={index}"))
                    })
                })
                .collect();
            for job in jobs {
                job.join().unwrap();
            }
        });
    }
    let mut seen = validations.lock().unwrap().clone();
    seen.sort();
    assert_eq!(seen, ["profile=0", "profile=1"]);
    for owner in &owners {
        stop_partition(owner);
    }
    server.close();
}

#[test]
fn test_partition_websocket_handshakes_keep_their_cookie_owner() {
    use net_traits::request::{CacheMode, RedirectMode, RequestMode, ServiceWorkersMode};
    use net_traits::{FetchChannels, MessageData, WebSocketDomAction, WebSocketNetworkEvent};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = std::thread::spawn(move || {
        for _ in 0..2 {
            let (stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(30)))
                .unwrap();
            stream
                .set_write_timeout(Some(std::time::Duration::from_secs(30)))
                .unwrap();
            let mut cookie = String::new();
            let mut socket = tungstenite::accept_hdr(
                stream,
                |request: &tungstenite::handshake::server::Request, response| {
                    cookie = request.headers()[http::header::COOKIE]
                        .to_str()
                        .unwrap()
                        .to_owned();
                    Ok(response)
                },
            )
            .unwrap();
            socket
                .send(tungstenite::Message::Text(cookie.into()))
                .unwrap();
            // Complete the close handshake before allowing the next partition to connect.
            let _ = socket.read();
            let _ = socket.flush();
        }
    });
    let url = servo_url::ServoUrl::parse(&format!("http://127.0.0.1:{port}/socket")).unwrap();
    let ws_url = servo_url::ServoUrl::parse(&format!("ws://127.0.0.1:{port}/socket")).unwrap();
    let owners = [
        partition_resource_owner(None, None),
        partition_resource_owner(None, None),
    ];
    for (index, owner) in owners.iter().enumerate() {
        owner.set_cookie_for_url_sync(
            url.clone(),
            cookie::Cookie::new("profile", index.to_string()),
            net_traits::CookieSource::HTTP,
        );
        let (actions, action_receiver) = generic_channel::lazy_callback();
        let (sender, receiver) = std::sync::mpsc::channel();
        let event_sender = profile_traits::generic_callback::GenericCallback::new(move |event| {
            let _ = sender.send(event);
        })
        .unwrap();
        // The core converts ws/wss to http/https for Fetch. Passing http here
        // would take its wss branch and send TLS to this plaintext server.
        let request = partition_request(ws_url.clone())
            .origin(url.origin())
            .mode(RequestMode::WebSocket {
                protocols: Vec::new(),
                original_url: ws_url.clone(),
            })
            .service_workers_mode(ServiceWorkersMode::None)
            .cache_mode(CacheMode::NoCache)
            .redirect_mode(RedirectMode::Error);
        owner
            .core_thread
            .send(CoreResourceMsg::Fetch(
                request,
                FetchChannels::WebSocket {
                    event_sender,
                    action_receiver,
                },
            ))
            .unwrap();
        loop {
            match receiver
                .recv_timeout(std::time::Duration::from_secs(30))
                .unwrap()
                .unwrap()
            {
                WebSocketNetworkEvent::ConnectionEstablished { .. } => {},
                WebSocketNetworkEvent::MessageReceived(MessageData::Text(value)) => {
                    assert_eq!(value, format!("profile={index}"));
                    actions.send(WebSocketDomAction::Close(None, None)).unwrap();
                    break;
                },
                event => panic!("unexpected WebSocket event: {event:?}"),
            }
        }
    }
    server.join().unwrap();
    for owner in &owners {
        stop_partition(owner);
    }
}

#[test]
fn test_partition_preloads_do_not_share_response_entries() {
    use net::test_util::{make_body, make_server};
    use net_traits::request::{
        InsecureRequestsPolicy, Origin, PreloadId, PreloadKey, RequestClient,
    };
    let (server, url) = make_server(|request, response| {
        response.headers_mut().insert(
            http::header::CACHE_CONTROL,
            http::HeaderValue::from_static("no-store"),
        );
        *response.body_mut() =
            make_body(request.headers()[http::header::COOKIE].as_bytes().to_vec());
    });
    let url = servo_url::ServoUrl::parse(url.as_str()).unwrap();
    let owners = [
        partition_resource_owner(None, None),
        partition_resource_owner(None, None),
    ];
    // Even identical transport identifiers must resolve only in their owning core.
    let preload_id = PreloadId::default();
    for (index, owner) in owners.iter().enumerate() {
        owner.set_cookie_for_url_sync(
            url.clone(),
            cookie::Cookie::new("profile", index.to_string()),
            net_traits::CookieSource::HTTP,
        );
        assert_eq!(
            partition_fetch_request(
                owner,
                partition_request(url.clone()).preload_id(preload_id.clone())
            ),
            format!("profile={index}")
        );
        owner.set_cookie_for_url_sync(
            url.clone(),
            cookie::Cookie::new("profile", "changed"),
            net_traits::CookieSource::HTTP,
        );
    }
    for (index, owner) in owners.iter().enumerate() {
        let request = partition_request(url.clone());
        let client = RequestClient {
            preloaded_resources: [(PreloadKey::new(&request), preload_id.clone())]
                .into_iter()
                .collect(),
            policy_container: Default::default(),
            origin: Origin::Origin(url.origin()),
            is_nested_browsing_context: false,
            insecure_requests_policy: InsecureRequestsPolicy::DoNotUpgrade,
            has_trustworthy_ancestor_origin: true,
            is_window: true,
        };
        assert_eq!(
            partition_fetch_request(owner, request.client(client)),
            format!("profile={index}")
        );
        stop_partition(owner);
    }
    server.close();
}

#[test]
fn test_partition_seeded_auth_cache_is_isolated_and_ephemeral_has_none() {
    use headers::{Authorization, HeaderMapExt};
    use net::resource_thread::{AuthCache, AuthCacheEntry};
    use net::test_util::{make_body, make_server};
    let (server, url) = make_server(|request, response| {
        response.headers_mut().insert(
            http::header::CACHE_CONTROL,
            http::HeaderValue::from_static("no-store"),
        );
        // Echo every Authorization value so an extra or foreign header also fails.
        let authorization = request
            .headers()
            .get_all(http::header::AUTHORIZATION)
            .iter()
            .map(|value| value.to_str().unwrap())
            .collect::<Vec<_>>()
            .join("\n");
        *response.body_mut() = make_body(authorization.into_bytes());
    });
    let url = servo_url::ServoUrl::parse(url.as_str()).unwrap();
    let roots = [tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap()];
    let mut expected = Vec::new();
    for (index, root) in roots.iter().enumerate() {
        let username = format!("profile-{index}");
        let password = format!("test-password-{index}");
        let mut headers = http::HeaderMap::new();
        headers.typed_insert(Authorization::basic(&username, &password));
        expected.push(
            headers[http::header::AUTHORIZATION]
                .to_str()
                .unwrap()
                .to_owned(),
        );
        let mut auth = AuthCache::default();
        auth.entries.insert(
            url.origin().ascii_serialization().into_owned(),
            AuthCacheEntry {
                user_name: username,
                password,
            },
        );
        // Seed the existing startup format with synthetic test credentials.
        // S1 must not implement population of the cache after authentication.
        servo_base::write_json_to_file(&auth, root.path(), "auth_cache.json");
    }
    let owners = roots
        .each_ref()
        .map(|root| partition_resource_owner(Some(root.path().to_owned()), None));
    let ephemeral = partition_resource_owner(None, None);
    for _ in 0..2 {
        assert_eq!(partition_fetch(&ephemeral, url.clone()), "");
        for (owner, expected_header) in owners.iter().zip(&expected) {
            assert_eq!(partition_fetch(owner, url.clone()), *expected_header);
        }
        assert_eq!(partition_fetch(&ephemeral, url.clone()), "");
    }
    for owner in &owners {
        stop_partition(owner);
    }
    stop_partition(&ephemeral);
    server.close();
}
