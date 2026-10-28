/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Embedder notifications for requests that are in flight.
//!
//! Each test checks the same invariant on the recorded log: every started id
//! has exactly one later finished, no finished lacks a started, and a second
//! start for an id arrives only after that id has finished.

mod common;

use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use http::header::{self, HeaderValue};
use http::{Method, StatusCode};
use http_body_util::BodyExt;
use http_body_util::combinators::BoxBody;
use hyper::body::{Bytes, Incoming};
use hyper::{Request as HyperRequest, Response as HyperResponse};
use net::async_runtime::spawn_task;
use net::test_util::{
    HeldResponseBody, Server, held_response_body, make_body, make_loopback_server,
};
use servo::{
    JSValue, LoadStatus, RequestId, WebResourceRequest, WebView, WebViewBuilder, WebViewDelegate,
};
use url::Url;

use crate::common::{ServoTest, evaluate_javascript};

const PAGE: &[u8] = b"<!DOCTYPE html><title>page</title><p>ok</p>";
const TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Clone, Debug)]
enum ActivityEvent {
    Started {
        id: RequestId,
        request: WebResourceRequest,
    },
    Finished {
        id: RequestId,
    },
}

struct ActivityDelegate {
    events: RefCell<Vec<ActivityEvent>>,
}

impl ActivityDelegate {
    fn summary(&self) -> String {
        format_events(&self.events.borrow())
    }

    fn assert_consistent(&self, expect_settled: bool) {
        let events = self.events.borrow();
        let mut open = HashSet::new();
        for event in events.iter() {
            match event {
                ActivityEvent::Started { id, .. } => {
                    assert!(
                        open.insert(*id),
                        "second start while {id:?} was in flight\n{}",
                        format_events(&events)
                    );
                },
                ActivityEvent::Finished { id } => {
                    assert!(
                        open.remove(id),
                        "finished without a matching start: {id:?}\n{}",
                        format_events(&events)
                    );
                },
            }
        }
        if expect_settled {
            assert!(
                open.is_empty(),
                "requests still in flight: {open:?}\n{}",
                format_events(&events)
            );
        }
    }

    fn open_requests(&self) -> Vec<(RequestId, WebResourceRequest)> {
        let mut open = Vec::new();
        for event in self.events.borrow().iter() {
            match event {
                ActivityEvent::Started { id, request } => open.push((*id, request.clone())),
                ActivityEvent::Finished { id } => {
                    if let Some(index) = open.iter().position(|(open_id, _)| open_id == id) {
                        open.remove(index);
                    }
                },
            }
        }
        open
    }

    fn finished_count(&self, id: RequestId) -> usize {
        self.events
            .borrow()
            .iter()
            .filter(|event| {
                matches!(event, ActivityEvent::Finished { id: finished } if *finished == id)
            })
            .count()
    }

    fn len(&self) -> usize {
        self.events.borrow().len()
    }
}

impl WebViewDelegate for ActivityDelegate {
    fn notify_request_started(
        &self,
        _webview: WebView,
        id: RequestId,
        request: WebResourceRequest,
    ) {
        self.events
            .borrow_mut()
            .push(ActivityEvent::Started { id, request });
    }

    fn notify_request_finished(&self, _webview: WebView, id: RequestId) {
        self.events
            .borrow_mut()
            .push(ActivityEvent::Finished { id });
    }
}

struct Loopback {
    _server: Server,
    origin: String,
    held: Option<HeldResponseBody>,
}

impl Loopback {
    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.origin)
    }

    fn release_upload(&mut self) {
        if let Some(held) = self.held.take() {
            held.release();
        }
    }
}

/// Servo must already be running: it owns the async runtime this server uses.
fn start_loopback() -> Loopback {
    let (held, upload_body) = held_response_body();
    let upload_body = Arc::new(Mutex::new(Some(upload_body)));
    let handler = {
        let upload_body = upload_body.clone();
        move |request: HyperRequest<Incoming>,
              response: &mut HyperResponse<BoxBody<Bytes, hyper::Error>>| {
            let (parts, body) = request.into_parts();
            // Read the upload without blocking the handler, so the response
            // headers can go out while the body above stays open.
            spawn_task(async move {
                let _ = BodyExt::collect(body).await;
            });
            match (parts.method, parts.uri.path()) {
                (Method::POST, "/upload") => {
                    *response.body_mut() = upload_body
                        .lock()
                        .expect("upload body lock")
                        .take()
                        .expect("only one held upload");
                },
                (Method::GET, "/go") => write_redirect(response, "/page"),
                (Method::GET, "/redirect") => write_redirect(response, "/landed"),
                _ => write_html(response, PAGE),
            }
        }
    };
    let (server, url) = make_loopback_server(handler);
    Loopback {
        _server: server,
        origin: format!("http://127.0.0.1:{}", url.port().expect("loopback port")),
        held: Some(held),
    }
}

fn write_html(response: &mut HyperResponse<BoxBody<Bytes, hyper::Error>>, body: &[u8]) {
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html;charset=utf-8"),
    );
    *response.body_mut() = make_body(body.to_vec());
}

fn write_redirect(response: &mut HyperResponse<BoxBody<Bytes, hyper::Error>>, location: &str) {
    *response.status_mut() = StatusCode::FOUND;
    response.headers_mut().insert(
        header::LOCATION,
        HeaderValue::from_str(location).expect("redirect location"),
    );
    *response.body_mut() = make_body(Vec::new());
}

fn spin_until(
    servo_test: &ServoTest,
    delegate: &ActivityDelegate,
    what: &str,
    done: impl FnMut() -> bool,
) {
    // `ServoTest::spin` takes `Fn`, so the predicate's mutable state lives in a cell.
    let started = Instant::now();
    let done = RefCell::new(done);
    servo_test.spin(|| {
        if (done.borrow_mut())() {
            return false;
        }
        if started.elapsed() > TIMEOUT {
            panic!(
                "timed out waiting for {what} after {TIMEOUT:?}\n{}",
                delegate.summary()
            );
        }
        true
    });
}

fn wait_until_settled(servo_test: &ServoTest, webview: &WebView, delegate: &ActivityDelegate) {
    let webview = webview.clone();
    spin_until(servo_test, delegate, "the page load to finish", || {
        webview.load_status() == LoadStatus::Complete && delegate.open_requests().is_empty()
    });
    let mut quiet_turns = 0;
    spin_until(servo_test, delegate, "request activity to go quiet", || {
        delegate.assert_consistent(false);
        if delegate.open_requests().is_empty() {
            quiet_turns += 1;
        } else {
            quiet_turns = 0;
        }
        quiet_turns >= 30
    });
    delegate.assert_consistent(true);
}

fn open_page(servo_test: &ServoTest, delegate: &Rc<ActivityDelegate>, url: &str) -> WebView {
    let webview = WebViewBuilder::new(servo_test.servo(), servo_test.rendering_context.clone())
        .delegate(delegate.clone())
        .url(Url::parse(url).expect("page url"))
        .build();
    wait_until_settled(servo_test, &webview, delegate);
    webview
}

fn run_script(servo_test: &ServoTest, webview: &WebView, script: String) {
    assert_eq!(
        evaluate_javascript(servo_test, webview.clone(), script),
        Ok(JSValue::Boolean(true)),
        "script failed"
    );
}

fn fetch_upload_script(url: &str) -> String {
    format!(
        "fetch({url:?}, {{ method: 'POST', body: new Uint8Array(1 << 20) }}).catch(function() {{}}); true;"
    )
}

fn xhr_upload_script(url: &str) -> String {
    format!(
        "var xhr = new XMLHttpRequest(); xhr.open('POST', {url:?}); xhr.send(new Uint8Array(1 << 20)); true;"
    )
}

fn abortable_upload_script(url: &str) -> String {
    format!(
        "globalThis.__requestActivityAbort = new AbortController(); \
         fetch({url:?}, {{ method: 'POST', body: new Uint8Array(1 << 20), signal: globalThis.__requestActivityAbort.signal }}).catch(function() {{}}); \
         true;"
    )
}

fn wait_for_single_open_post(servo_test: &ServoTest, delegate: &ActivityDelegate) -> RequestId {
    let mut found = None;
    spin_until(
        servo_test,
        delegate,
        "one in-flight POST with a body",
        || {
            delegate.assert_consistent(false);
            let posts: Vec<_> = delegate
                .open_requests()
                .into_iter()
                .filter(|(_, request)| request.method == Method::POST && request.has_body)
                .map(|(id, _)| id)
                .collect();
            match posts.len() {
                0 => false,
                1 => {
                    found = Some(posts[0]);
                    true
                },
                _ => panic!("more than one POST in flight\n{}", delegate.summary()),
            }
        },
    );
    let id = found.expect("spin found a POST");
    assert_eq!(
        delegate.finished_count(id),
        0,
        "POST finished while its response was still held\n{}",
        delegate.summary()
    );
    id
}

fn wait_for_single_finish(servo_test: &ServoTest, delegate: &ActivityDelegate, id: RequestId) {
    spin_until(
        servo_test,
        delegate,
        "the held request to finish once",
        || {
            let finished = delegate.finished_count(id);
            if finished > 1 {
                panic!("request finished more than once\n{}", delegate.summary());
            }
            finished == 1
                && delegate
                    .open_requests()
                    .iter()
                    .all(|(open_id, _)| *open_id != id)
        },
    );
}

fn assert_one_settled_pair(
    delegate: &ActivityDelegate,
    mark: usize,
    predicate: impl Fn(&WebResourceRequest) -> bool,
) {
    let events = delegate.events.borrow();
    let new_events = &events[mark..];
    assert_eq!(
        new_events.len(),
        2,
        "expected one new pair\n{}",
        format_events(&events)
    );
    match &new_events[0] {
        ActivityEvent::Started { id, request } => {
            assert!(
                predicate(request),
                "unexpected request {} {}\n{}",
                request.method,
                request.url,
                format_events(&events)
            );
            match &new_events[1] {
                ActivityEvent::Finished { id: finished } => assert_eq!(id, finished),
                ActivityEvent::Started { .. } => {
                    panic!("second start before finished\n{}", format_events(&events))
                },
            }
        },
        ActivityEvent::Finished { .. } => {
            panic!("finished before started\n{}", format_events(&events))
        },
    }
}

fn format_events(events: &[ActivityEvent]) -> String {
    if events.is_empty() {
        return "(no request activity)".to_owned();
    }
    events
        .iter()
        .map(|event| match event {
            ActivityEvent::Started { id, request } => format!(
                "started {id:?} {} {} has_body={} destination={:?}",
                request.method, request.url, request.has_body, request.destination
            ),
            ActivityEvent::Finished { id } => format!("finished {id:?}"),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn new_delegate() -> Rc<ActivityDelegate> {
    Rc::new(ActivityDelegate {
        events: RefCell::default(),
    })
}

fn held_upload_finishes_when_released(script: fn(&str) -> String) {
    let servo_test = ServoTest::new();
    let delegate = new_delegate();
    let mut server = start_loopback();
    let webview = open_page(&servo_test, &delegate, &server.url("/"));
    run_script(&servo_test, &webview, script(&server.url("/upload")));
    let id = wait_for_single_open_post(&servo_test, &delegate);
    server.release_upload();
    wait_for_single_finish(&servo_test, &delegate, id);
    wait_until_settled(&servo_test, &webview, &delegate);
}

#[test]
fn test_fetch_upload_after_load() {
    held_upload_finishes_when_released(fetch_upload_script);
}

#[test]
fn test_xhr_upload_after_load() {
    held_upload_finishes_when_released(xhr_upload_script);
}

#[test]
fn test_abort_fetch_upload() {
    let servo_test = ServoTest::new();
    let delegate = new_delegate();
    let server = start_loopback();
    let webview = open_page(&servo_test, &delegate, &server.url("/"));
    run_script(
        &servo_test,
        &webview,
        abortable_upload_script(&server.url("/upload")),
    );
    let id = wait_for_single_open_post(&servo_test, &delegate);
    run_script(
        &servo_test,
        &webview,
        "globalThis.__requestActivityAbort.abort(); true;".to_owned(),
    );
    wait_for_single_finish(&servo_test, &delegate, id);
    wait_until_settled(&servo_test, &webview, &delegate);
}

#[test]
fn test_fetch_network_error() {
    let servo_test = ServoTest::new();
    let delegate = new_delegate();
    let server = start_loopback();
    let webview = open_page(&servo_test, &delegate, &server.url("/"));
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind closed port");
    let port = listener.local_addr().expect("closed port").port();
    drop(listener);
    let mark = delegate.len();
    let url = format!("http://127.0.0.1:{port}/");
    run_script(
        &servo_test,
        &webview,
        format!("fetch({url:?}).catch(function() {{}}); true;"),
    );
    spin_until(&servo_test, &delegate, "the failed fetch to finish", || {
        delegate.len() >= mark + 2 && delegate.open_requests().is_empty()
    });
    assert_one_settled_pair(&delegate, mark, |request| request.url.port() == Some(port));
    delegate.assert_consistent(true);
}

#[test]
fn test_subresource_redirect() {
    let servo_test = ServoTest::new();
    let delegate = new_delegate();
    let server = start_loopback();
    let webview = open_page(&servo_test, &delegate, &server.url("/"));
    let mark = delegate.len();
    let url = server.url("/redirect");
    run_script(
        &servo_test,
        &webview,
        format!("fetch({url:?}).catch(function() {{}}); true;"),
    );
    spin_until(
        &servo_test,
        &delegate,
        "the redirected fetch to finish",
        || delegate.len() >= mark + 2 && delegate.open_requests().is_empty(),
    );
    assert_one_settled_pair(&delegate, mark, |request| request.url.path() == "/redirect");
    delegate.assert_consistent(true);
}

#[test]
fn test_document_redirect() {
    let servo_test = ServoTest::new();
    let delegate = new_delegate();
    let server = start_loopback();
    let _webview = open_page(&servo_test, &delegate, &server.url("/go"));
    let saw_redirect = delegate.events.borrow().iter().any(|event| {
        matches!(
            event,
            ActivityEvent::Started { request, .. } if request.url.path() == "/go"
        )
    });
    assert!(
        saw_redirect,
        "document load did not report the redirecting request\n{}",
        delegate.summary()
    );
    delegate.assert_consistent(true);
}

#[test]
fn test_navigate_away_during_upload() {
    let servo_test = ServoTest::new();
    let delegate = new_delegate();
    let server = start_loopback();
    let webview = open_page(&servo_test, &delegate, &server.url("/"));
    run_script(
        &servo_test,
        &webview,
        fetch_upload_script(&server.url("/upload")),
    );
    let id = wait_for_single_open_post(&servo_test, &delegate);
    // Script navigation aborts the document, which cancels this fetch.
    // `WebView::load` goes straight to constellation `load_url` and does not.
    let next = server.url("/next");
    run_script(
        &servo_test,
        &webview,
        format!("location.href = {next:?}; true;"),
    );
    wait_for_single_finish(&servo_test, &delegate, id);
    wait_until_settled(&servo_test, &webview, &delegate);
}

#[test]
fn test_data_url_fetch() {
    let servo_test = ServoTest::new();
    let delegate = new_delegate();
    let server = start_loopback();
    let webview = open_page(&servo_test, &delegate, &server.url("/"));
    let mark = delegate.len();
    run_script(
        &servo_test,
        &webview,
        "fetch('data:text/plain,hi').catch(function() {}); true;".to_owned(),
    );
    spin_until(&servo_test, &delegate, "the data: fetch to finish", || {
        delegate.len() >= mark + 2 && delegate.open_requests().is_empty()
    });
    assert_one_settled_pair(&delegate, mark, |request| request.url.scheme() == "data");
    delegate.assert_consistent(true);
}
