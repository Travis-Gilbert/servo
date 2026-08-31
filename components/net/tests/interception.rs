/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use content_security_policy::{CspList, PolicyDisposition, PolicySource};
use crossbeam_channel::{Receiver, TryRecvError, bounded};
use embedder_traits::{WebResourceRequest, WebResourceResponse, WebResourceResponseMsg};
use flate2::Compression;
use flate2::write::GzEncoder;
use http::header::{self, HeaderMap, HeaderName, HeaderValue};
use http::{Method, StatusCode};
use http_body_util::combinators::BoxBody;
use hyper::body::{Bytes, Incoming};
use hyper::{Request as HyperRequest, Response as HyperResponse};
use net::async_runtime::spawn_blocking_task;
use net::embedder::NetToEmbedderMsg;
use net::fetch::methods::{CancellationListener, FetchContext};
use net::test_util::Server;
use net_traits::blob_url_store::UrlWithBlobClaim;
use net_traits::policy_container::PolicyContainer;
use net_traits::request::{
    CacheMode, CredentialsMode, Destination, Referrer, Request, RequestBuilder, RequestMode,
    create_request_body_with_content,
};
use net_traits::response::{CacheState, Response, ResponseBody, ResponseType};
use net_traits::{CookieSource, NetworkError};
use servo_base::id::TEST_WEBVIEW_ID;
use servo_url::ServoUrl;
use tokio::sync::mpsc::UnboundedSender;

use crate::{
    create_generic_embedder_proxy_and_receiver, fetch_with_context, make_body, make_server,
    new_fetch_context,
};

const TEST_TIMEOUT: Duration = Duration::from_secs(10);
const NETWORK_BODY: &[u8] = b"response from the fallback HTTP server";
type ResponseSender = UnboundedSender<WebResourceResponseMsg>;

/// The real HTTP server is a tripwire: a rejected claimed response must not fall back to it.
struct InterceptionTest {
    context: FetchContext,
    requests: Receiver<NetToEmbedderMsg>,
    url: ServoUrl,
    network_requests: Arc<AtomicUsize>,
    _server: Server,
}

impl InterceptionTest {
    fn new() -> Self {
        let network_requests = Arc::new(AtomicUsize::new(0));
        let counter = network_requests.clone();
        // make_server also initializes the shared async runtime used by new_fetch_context.
        let (server, url) = make_server(
            move |_: HyperRequest<Incoming>,
                  response: &mut HyperResponse<BoxBody<Bytes, hyper::Error>>| {
                counter.fetch_add(1, Ordering::SeqCst);
                *response.body_mut() = make_body(NETWORK_BODY.to_vec());
            },
        );
        let (proxy, requests) = create_generic_embedder_proxy_and_receiver();
        Self {
            context: new_fetch_context(None, Some(proxy)),
            requests,
            url: url.url(),
            network_requests,
            _server: server,
        }
    }

    fn request(&self) -> RequestBuilder {
        RequestBuilder::new(
            Some(TEST_WEBVIEW_ID),
            UrlWithBlobClaim::new(self.url.clone(), None),
            Referrer::NoReferrer,
        )
        .origin(self.url.origin())
        .mode(RequestMode::SameOrigin)
        .cache_mode(CacheMode::NoStore)
        .policy_container(Default::default())
    }

    fn cors_request(&self) -> RequestBuilder {
        self.request()
            .origin(ServoUrl::parse("http://caller.invalid/").unwrap().origin())
            .mode(RequestMode::CorsMode)
            .credentials_mode(CredentialsMode::Omit)
    }

    fn fetch(&self, request: Request) -> PendingFetch {
        let mut context = self.context.clone();
        // Separate cancellation from the shared cache/cookie state between sequential fetches.
        context.cancellation_listener = Arc::new(CancellationListener::default());
        let cancellation_listener = context.cancellation_listener.clone();
        let (sender, response) = bounded(1);
        std::thread::spawn(move || {
            let result = fetch_with_context(request, &mut context);
            let _ = sender.send(result);
        });
        PendingFetch {
            response,
            cancellation_listener,
        }
    }

    fn next_request(&self) -> (WebResourceRequest, ResponseSender) {
        let message = self
            .requests
            .recv_timeout(TEST_TIMEOUT)
            .expect("fetch must dispatch an interception within the deadline");
        let NetToEmbedderMsg::WebResourceRequested(webview, request, sender) = message else {
            panic!("expected a web resource interception");
        };
        assert_eq!(webview, Some(TEST_WEBVIEW_ID));
        (request, sender)
    }

    fn assert_no_more_requests(&self) {
        assert!(matches!(self.requests.try_recv(), Err(TryRecvError::Empty)));
    }

    fn assert_no_network(&self) {
        assert_eq!(self.network_requests.load(Ordering::SeqCst), 0);
    }
}

struct PendingFetch {
    response: Receiver<Response>,
    cancellation_listener: Arc<CancellationListener>,
}

impl PendingFetch {
    fn finish(self) -> Response {
        self.response
            .recv_timeout(TEST_TIMEOUT)
            .expect("fetch must deliver response EOF within the deadline")
    }
}

impl Drop for PendingFetch {
    fn drop(&mut self) {
        // Also release pending interception waits when a regression makes an assertion fail.
        self.cancellation_listener.cancel();
    }
}

fn headers(values: &[(&str, &str)]) -> HeaderMap {
    values
        .iter()
        .map(|(name, value)| {
            (
                HeaderName::from_bytes(name.as_bytes()).unwrap(),
                HeaderValue::from_str(value).unwrap(),
            )
        })
        .collect()
}

fn send(sender: &ResponseSender, message: WebResourceResponseMsg) {
    assert!(
        sender.send(message).is_ok(),
        "interception must still be open"
    );
}

fn respond(sender: &ResponseSender, response: WebResourceResponse, chunks: &[&[u8]]) {
    send(sender, WebResourceResponseMsg::Start(response));
    for chunk in chunks {
        send(sender, WebResourceResponseMsg::SendBodyData(chunk.to_vec()));
    }
    send(sender, WebResourceResponseMsg::FinishLoad);
}

fn assert_body(response: &Response, expected: &[u8]) {
    assert!(
        !response.is_network_error(),
        "{:?}",
        response.get_network_error()
    );
    assert_eq!(*response.body.lock(), ResponseBody::Done(expected.to_vec()));
}

fn assert_resource_error(response: &Response) {
    assert!(
        matches!(
            response.get_network_error(),
            Some(NetworkError::ResourceLoadError(_))
        ),
        "expected an interception protocol error, got {:?}",
        response.get_network_error(),
    );
}

fn assert_no_body(response: &Response) {
    assert!(
        !response.is_network_error(),
        "{:?}",
        response.get_network_error()
    );
    // Fetch represents a null body as Empty; a completed empty transport may also use Done([]).
    match &*response.body.lock() {
        ResponseBody::Empty => {},
        ResponseBody::Done(body) => assert!(body.is_empty()),
        ResponseBody::Receiving(_) => panic!("response EOF must not leave a receiving body"),
    }
}

#[test]
fn same_origin_response_preserves_custom_status_headers_and_body() {
    let test = InterceptionTest::new();
    let fetch = test.fetch(test.request().build());
    let (request, sender) = test.next_request();
    assert_eq!(request.method, Method::GET);
    assert!(!request.has_body);
    assert_eq!(request.url, test.url.clone().into_url());
    assert!(!request.is_redirect);
    assert!(request.headers.contains_key(header::USER_AGENT));
    respond(
        &sender,
        WebResourceResponse::new(request.url)
            .status_code(StatusCode::from_u16(299).unwrap())
            .status_message(b"Custom intercepted status".to_vec())
            .headers(headers(&[("x-intercepted", "yes")])),
        &[b"intercepted ", b"body"],
    );

    let response = fetch.finish();
    assert_eq!(response.response_type, ResponseType::Basic);
    assert_eq!(response.status.raw_code(), 299);
    assert_eq!(response.status.message(), b"Custom intercepted status");
    assert_eq!(response.headers["x-intercepted"], "yes");
    assert_body(&response, b"intercepted body");
    test.assert_no_network();
}

#[test]
fn empty_response_finishes_without_a_body_chunk() {
    let test = InterceptionTest::new();
    let fetch = test.fetch(test.request().build());
    let (request, sender) = test.next_request();
    respond(&sender, WebResourceResponse::new(request.url), &[]);

    let response = fetch.finish();
    assert_eq!(response.status.code(), StatusCode::OK);
    assert_body(&response, b"");
    test.assert_no_network();
}

#[test]
fn cross_origin_response_without_allow_origin_is_rejected() {
    let test = InterceptionTest::new();
    let fetch = test.fetch(test.cors_request().build());
    let (request, sender) = test.next_request();
    respond(
        &sender,
        WebResourceResponse::new(request.url),
        &[b"private"],
    );

    assert_eq!(
        fetch.finish().get_network_error(),
        Some(&NetworkError::CorsGeneral)
    );
    test.assert_no_network();
}

#[test]
fn cors_response_exposes_only_permitted_headers() {
    let test = InterceptionTest::new();
    let fetch = test.fetch(test.cors_request().build());
    let (request, sender) = test.next_request();
    assert_eq!(request.headers[header::ORIGIN], "http://caller.invalid");
    respond(
        &sender,
        WebResourceResponse::new(request.url).headers(headers(&[
            ("access-control-allow-origin", "*"),
            ("access-control-expose-headers", "x-public"),
            ("x-public", "visible"),
            ("x-private", "hidden"),
        ])),
        &[b"allowed"],
    );

    let response = fetch.finish();
    assert_eq!(response.response_type, ResponseType::Cors);
    assert_eq!(response.headers["x-public"], "visible");
    assert!(!response.headers.contains_key("x-private"));
    assert_eq!(response.actual_response().headers["x-private"], "hidden");
    assert_body(&response, b"allowed");
    test.assert_no_network();
}

#[test]
fn no_cors_response_is_opaque() {
    let test = InterceptionTest::new();
    let fetch = test.fetch(test.cors_request().mode(RequestMode::NoCors).build());
    let (request, sender) = test.next_request();
    respond(
        &sender,
        WebResourceResponse::new(request.url).headers(headers(&[("x-private", "hidden")])),
        &[b"not visible to the caller"],
    );

    let response = fetch.finish();
    assert_eq!(response.response_type, ResponseType::Opaque);
    assert_eq!(response.status.raw_code(), 0);
    assert!(response.headers.is_empty());
    assert!(response.url_list.is_empty());
    assert_eq!(*response.body.lock(), ResponseBody::Empty);
    assert_body(response.actual_response(), b"not visible to the caller");
    test.assert_no_network();
}

#[test]
fn csp_blocked_request_never_reaches_the_interceptor() {
    let test = InterceptionTest::new();
    let policy = PolicyContainer {
        csp_list: Some(CspList::parse(
            "script-src 'none'",
            PolicySource::Header,
            PolicyDisposition::Enforce,
        )),
        ..Default::default()
    };
    let fetch = test.fetch(
        test.request()
            .destination(Destination::Script)
            .policy_container(policy)
            .build(),
    );

    assert_eq!(
        fetch.finish().get_network_error(),
        Some(&NetworkError::ContentSecurityPolicy),
    );
    test.assert_no_more_requests();
    test.assert_no_network();
}

#[test]
fn same_origin_mode_blocks_cross_origin_before_interception() {
    let test = InterceptionTest::new();
    let fetch = test.fetch(test.cors_request().mode(RequestMode::SameOrigin).build());

    assert_eq!(
        fetch.finish().get_network_error(),
        Some(&NetworkError::CrossOriginResponse),
    );
    test.assert_no_more_requests();
    test.assert_no_network();
}

#[test]
fn redirect_dispatches_the_current_url_and_retains_url_history() {
    let test = InterceptionTest::new();
    let destination = test.url.join("redirect-target#fragment").unwrap();
    let fetch = test.fetch(test.request().build());
    let (request, sender) = test.next_request();
    respond(
        &sender,
        WebResourceResponse::new(request.url)
            .status_code(StatusCode::FOUND)
            .headers(headers(&[("location", destination.as_str())])),
        &[],
    );

    let (redirect, sender) = test.next_request();
    assert_eq!(redirect.url, destination.clone().into_url());
    assert!(redirect.is_redirect);
    respond(
        &sender,
        WebResourceResponse::new(redirect.url),
        &[b"redirected"],
    );

    let response = fetch.finish();
    assert_eq!(response.actual_response().url(), Some(&destination));
    assert_eq!(
        response.actual_response().url_list,
        vec![test.url.clone(), destination],
    );
    assert_body(&response, b"redirected");
    test.assert_no_more_requests();
    test.assert_no_network();
}

#[test]
fn response_url_may_differ_only_in_fragment() {
    let test = InterceptionTest::new();
    let fetch = test.fetch(test.request().build());
    let (request, sender) = test.next_request();
    let mut response_url = request.url;
    response_url.set_fragment(Some("not-a-network-resource-change"));
    respond(
        &sender,
        WebResourceResponse::new(response_url),
        &[b"same resource"],
    );

    let response = fetch.finish();
    assert_eq!(response.actual_response().url(), Some(&test.url));
    assert_body(&response, b"same resource");
    test.assert_no_network();
}

#[test]
fn mismatched_response_url_is_rejected_without_network_fallback() {
    let test = InterceptionTest::new();
    let fetch = test.fetch(test.request().build());
    let (request, sender) = test.next_request();
    let mut response_url = request.url;
    response_url.set_path("/different-resource");
    send(
        &sender,
        WebResourceResponseMsg::Start(WebResourceResponse::new(response_url)),
    );

    assert_resource_error(&fetch.finish());
    test.assert_no_network();
}

#[test]
fn denied_preflight_prevents_actual_request_dispatch() {
    let test = InterceptionTest::new();
    let fetch = test.fetch(
        test.cors_request()
            .method(Method::PUT)
            .use_cors_preflight(true)
            .build(),
    );
    let (preflight, sender) = test.next_request();
    assert_eq!(preflight.method, Method::OPTIONS);
    assert_eq!(
        preflight.headers[header::ACCESS_CONTROL_REQUEST_METHOD],
        "PUT"
    );
    respond(&sender, WebResourceResponse::new(preflight.url), &[]);

    assert_eq!(
        fetch.finish().get_network_error(),
        Some(&NetworkError::CorsGeneral)
    );
    test.assert_no_more_requests();
    test.assert_no_network();
}

#[test]
fn allowed_preflight_dispatches_and_filters_actual_response() {
    let test = InterceptionTest::new();
    let fetch = test.fetch(
        test.cors_request()
            .method(Method::PUT)
            .headers(headers(&[("x-probe", "present")]))
            .unsafe_request(true)
            .use_cors_preflight(true)
            .build(),
    );
    let (preflight, sender) = test.next_request();
    assert_eq!(preflight.method, Method::OPTIONS);
    assert_eq!(
        preflight.headers[header::ACCESS_CONTROL_REQUEST_METHOD],
        "PUT"
    );
    assert_eq!(
        preflight.headers[header::ACCESS_CONTROL_REQUEST_HEADERS],
        "x-probe"
    );
    respond(
        &sender,
        WebResourceResponse::new(preflight.url)
            .status_code(StatusCode::NO_CONTENT)
            .headers(headers(&[
                ("access-control-allow-origin", "*"),
                ("access-control-allow-methods", "PUT"),
                ("access-control-allow-headers", "x-probe"),
            ])),
        &[],
    );

    let (actual, sender) = test.next_request();
    assert_eq!(actual.method, Method::PUT);
    assert_eq!(actual.headers["x-probe"], "present");
    assert_eq!(actual.url, test.url.clone().into_url());
    respond(
        &sender,
        WebResourceResponse::new(actual.url)
            .headers(headers(&[("access-control-allow-origin", "*")])),
        &[b"preflight allowed"],
    );

    let response = fetch.finish();
    assert_eq!(response.response_type, ResponseType::Cors);
    assert_body(&response, b"preflight allowed");
    test.assert_no_more_requests();
    test.assert_no_network();
}

#[test]
fn intercepted_cookies_use_the_shared_jar_and_credentials_policy() {
    let test = InterceptionTest::new();
    let fetch = test.fetch(
        test.request()
            .credentials_mode(CredentialsMode::Include)
            .build(),
    );
    let (request, sender) = test.next_request();
    respond(
        &sender,
        WebResourceResponse::new(request.url).headers(headers(&[(
            "set-cookie",
            "intercepted=yes; Path=/; HttpOnly",
        )])),
        &[],
    );
    let response = fetch.finish();
    assert_body(&response, b"");
    assert!(!response.headers.contains_key(header::SET_COOKIE));
    assert_eq!(
        test.context
            .state
            .cookie_jar
            .write()
            .cookies_for_url(&test.url, CookieSource::HTTP),
        Some("intercepted=yes".to_owned()),
    );

    for credentials in [CredentialsMode::Include, CredentialsMode::Omit] {
        let fetch = test.fetch(test.request().credentials_mode(credentials).build());
        let (request, sender) = test.next_request();
        if credentials == CredentialsMode::Include {
            assert_eq!(request.headers[header::COOKIE], "intercepted=yes");
        } else {
            assert!(!request.headers.contains_key(header::COOKIE));
        }
        respond(&sender, WebResourceResponse::new(request.url), &[]);
        assert_body(&fetch.finish(), b"");
    }
    test.assert_no_network();
}

#[test]
fn omitted_credentials_do_not_store_intercepted_cookies() {
    let test = InterceptionTest::new();
    let fetch = test.fetch(
        test.request()
            .credentials_mode(CredentialsMode::Omit)
            .build(),
    );
    let (request, sender) = test.next_request();
    respond(
        &sender,
        WebResourceResponse::new(request.url)
            .headers(headers(&[("set-cookie", "must-not-be-stored=yes; Path=/")])),
        &[],
    );

    assert_body(&fetch.finish(), b"");
    assert_eq!(
        test.context
            .state
            .cookie_jar
            .write()
            .cookies_for_url(&test.url, CookieSource::HTTP),
        None,
    );
    test.assert_no_network();
}

#[test]
fn cacheable_intercepted_response_is_reused_without_redispatch() {
    let test = InterceptionTest::new();
    let request = test.request().cache_mode(CacheMode::Default).build();
    let fetch = test.fetch(request.clone());
    let (intercepted, sender) = test.next_request();
    respond(
        &sender,
        WebResourceResponse::new(intercepted.url)
            .headers(headers(&[("cache-control", "max-age=3600")])),
        &[b"cached body"],
    );
    let response = fetch.finish();
    assert!(matches!(response.cache_state, CacheState::None));
    assert_body(&response, b"cached body");

    let cached = test.fetch(request).finish();
    assert!(matches!(cached.cache_state, CacheState::Local));
    assert_body(&cached, b"cached body");
    test.assert_no_more_requests();
    test.assert_no_network();
}

#[test]
fn intercepted_gzip_body_uses_the_common_decoder() {
    let test = InterceptionTest::new();
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(b"decoded intercepted body").unwrap();
    let encoded = encoder.finish().unwrap();
    assert_ne!(encoded.len(), b"decoded intercepted body".len());
    let fetch = test.fetch(test.request().build());
    let (request, sender) = test.next_request();
    respond(
        &sender,
        WebResourceResponse::new(request.url).headers(headers(&[
            ("content-encoding", "gzip"),
            ("content-length", &encoded.len().to_string()),
        ])),
        &[&encoded],
    );

    assert_body(&fetch.finish(), b"decoded intercepted body");
    test.assert_no_network();
}

#[test]
fn intercepted_response_still_obeys_nosniff() {
    let test = InterceptionTest::new();
    let fetch = test.fetch(test.request().destination(Destination::Script).build());
    let (request, sender) = test.next_request();
    respond(
        &sender,
        WebResourceResponse::new(request.url).headers(headers(&[
            ("content-type", "text/plain"),
            ("x-content-type-options", "nosniff"),
        ])),
        &[b"not an executable MIME type"],
    );

    assert_eq!(
        fetch.finish().get_network_error(),
        Some(&NetworkError::Nosniff)
    );
    test.assert_no_network();
}

#[test]
fn body_data_before_start_is_rejected_without_fallback() {
    let test = InterceptionTest::new();
    let fetch = test.fetch(test.request().build());
    let (_, sender) = test.next_request();
    send(
        &sender,
        WebResourceResponseMsg::SendBodyData(b"out of order".to_vec()),
    );

    assert_resource_error(&fetch.finish());
    test.assert_no_network();
}

#[test]
fn finish_before_start_is_rejected_without_fallback() {
    let test = InterceptionTest::new();
    let fetch = test.fetch(test.request().build());
    let (_, sender) = test.next_request();
    send(&sender, WebResourceResponseMsg::FinishLoad);

    assert_resource_error(&fetch.finish());
    test.assert_no_network();
}

#[test]
fn duplicate_start_is_rejected_without_fallback() {
    let test = InterceptionTest::new();
    let fetch = test.fetch(test.request().build());
    let (request, sender) = test.next_request();
    let response = WebResourceResponse::new(request.url);
    send(&sender, WebResourceResponseMsg::Start(response.clone()));
    send(&sender, WebResourceResponseMsg::Start(response));

    assert_resource_error(&fetch.finish());
    test.assert_no_network();
}

#[test]
fn claimed_response_cannot_be_changed_to_do_not_intercept() {
    let test = InterceptionTest::new();
    let fetch = test.fetch(test.request().build());
    let (request, sender) = test.next_request();
    send(
        &sender,
        WebResourceResponseMsg::Start(WebResourceResponse::new(request.url)),
    );
    send(&sender, WebResourceResponseMsg::DoNotIntercept);

    assert_resource_error(&fetch.finish());
    test.assert_no_network();
}

#[test]
fn dropped_claimed_response_fails_instead_of_returning_partial_success() {
    for body_started in [false, true] {
        let test = InterceptionTest::new();
        let fetch = test.fetch(test.request().build());
        let (request, sender) = test.next_request();
        send(
            &sender,
            WebResourceResponseMsg::Start(WebResourceResponse::new(request.url)),
        );
        if body_started {
            send(
                &sender,
                WebResourceResponseMsg::SendBodyData(b"partial".to_vec()),
            );
        }
        drop(sender);

        assert_resource_error(&fetch.finish());
        test.assert_no_network();
    }
}

#[test]
fn unclaimed_response_uses_the_real_network_transport() {
    for explicit_decline in [false, true] {
        let test = InterceptionTest::new();
        let fetch = test.fetch(test.request().build());
        let (_, sender) = test.next_request();
        if explicit_decline {
            send(&sender, WebResourceResponseMsg::DoNotIntercept);
        }
        drop(sender);

        assert_body(&fetch.finish(), NETWORK_BODY);
        assert_eq!(test.network_requests.load(Ordering::SeqCst), 1);
    }
}

#[test]
fn interceptor_cancel_load_reports_cancellation() {
    for claimed in [false, true] {
        let test = InterceptionTest::new();
        let fetch = test.fetch(test.request().build());
        let (request, sender) = test.next_request();
        if claimed {
            send(
                &sender,
                WebResourceResponseMsg::Start(WebResourceResponse::new(request.url)),
            );
        }
        send(&sender, WebResourceResponseMsg::CancelLoad);

        assert_eq!(
            fetch.finish().get_network_error(),
            Some(&NetworkError::LoadCancelled)
        );
        test.assert_no_network();
    }
}

#[test]
fn cancellation_wakes_interception_waiting_for_headers() {
    let test = InterceptionTest::new();
    let fetch = test.fetch(test.request().build());
    // Keep the sender open: channel closure must not be what releases the pending receive.
    let (_, _sender) = test.next_request();
    fetch.cancellation_listener.cancel();

    assert_eq!(
        fetch.finish().get_network_error(),
        Some(&NetworkError::LoadCancelled)
    );
    test.assert_no_network();
}

#[test]
fn cancellation_wakes_claimed_response_without_finish() {
    let test = InterceptionTest::new();
    let fetch = test.fetch(test.request().build());
    let (request, sender) = test.next_request();
    send(
        &sender,
        WebResourceResponseMsg::Start(WebResourceResponse::new(request.url)),
    );
    send(
        &sender,
        WebResourceResponseMsg::SendBodyData(b"unfinished body".to_vec()),
    );
    // The adapter is buffered: cancel after body data is sent, without FinishLoad or closing.
    fetch.cancellation_listener.cancel();

    assert_eq!(
        fetch.finish().get_network_error(),
        Some(&NetworkError::LoadCancelled)
    );
    test.assert_no_network();
}

#[test]
fn claimed_request_with_upload_is_rejected_instead_of_sending_an_empty_body() {
    let test = InterceptionTest::new();
    let upload = "upload must not disappear";
    let fetch = test.fetch(
        test.request()
            .method(Method::POST)
            .body(Some(create_request_body_with_content(upload.to_owned())))
            .build(),
    );
    let (request, sender) = test.next_request();
    assert_eq!(request.method, Method::POST);
    assert!(request.has_body);
    assert_eq!(
        request.headers[header::CONTENT_LENGTH],
        upload.len().to_string().as_str()
    );
    send(
        &sender,
        WebResourceResponseMsg::Start(WebResourceResponse::new(request.url)),
    );

    assert_resource_error(&fetch.finish());
    test.assert_no_network();
}

#[test]
fn accumulated_encoded_body_over_limit_is_rejected_without_finish() {
    let test = InterceptionTest::new();
    let limit =
        usize::try_from(servo_config::pref!(network_interception_max_response_bytes)).unwrap();
    // Do not mutate process-global preferences while unrelated fetch tests run in parallel.
    assert_eq!(limit, 64 * 1024 * 1024);
    let fetch = test.fetch(test.request().build());
    let (request, sender) = test.next_request();
    send(
        &sender,
        WebResourceResponseMsg::Start(WebResourceResponse::new(request.url)),
    );
    send(
        &sender,
        WebResourceResponseMsg::SendBodyData(vec![b'x'; limit]),
    );
    send(&sender, WebResourceResponseMsg::SendBodyData(vec![b'x']));

    assert_resource_error(&fetch.finish());
    test.assert_no_network();
}

#[test]
fn oversized_declared_content_length_is_rejected_before_body_data() {
    let test = InterceptionTest::new();
    let limit =
        usize::try_from(servo_config::pref!(network_interception_max_response_bytes)).unwrap();
    let fetch = test.fetch(test.request().build());
    let (request, sender) = test.next_request();
    send(
        &sender,
        WebResourceResponseMsg::Start(
            WebResourceResponse::new(request.url)
                .headers(headers(&[("content-length", &(limit + 1).to_string())])),
        ),
    );

    assert_resource_error(&fetch.finish());
    test.assert_no_network();
}

#[test]
fn encoded_body_exactly_at_limit_is_accepted() {
    let test = InterceptionTest::new();
    let limit =
        usize::try_from(servo_config::pref!(network_interception_max_response_bytes)).unwrap();
    assert_eq!(limit, 64 * 1024 * 1024);
    let fetch = test.fetch(test.request().build());
    let (request, sender) = test.next_request();
    send(
        &sender,
        WebResourceResponseMsg::Start(WebResourceResponse::new(request.url)),
    );
    send(
        &sender,
        WebResourceResponseMsg::SendBodyData(vec![b'x'; limit]),
    );
    send(&sender, WebResourceResponseMsg::FinishLoad);

    let response = fetch.finish();
    assert!(
        !response.is_network_error(),
        "{:?}",
        response.get_network_error()
    );
    let body = response.body.lock();
    let ResponseBody::Done(body) = &*body else {
        panic!("body at the configured bound must finish");
    };
    assert_eq!(body.len(), limit);
    assert!(body.iter().all(|byte| *byte == b'x'));
    test.assert_no_network();
}

#[test]
fn invalid_content_length_is_rejected_before_body_data() {
    let test = InterceptionTest::new();
    for value in [
        "",
        "not-a-length",
        "-1",
        "1.5",
        "18446744073709551616",
        "1, 2",
    ] {
        let fetch = test.fetch(test.request().build());
        let (request, sender) = test.next_request();
        send(
            &sender,
            WebResourceResponseMsg::Start(
                WebResourceResponse::new(request.url)
                    .headers(headers(&[("content-length", value)])),
            ),
        );

        assert_resource_error(&fetch.finish());
    }
    test.assert_no_network();
}

#[test]
fn conflicting_content_length_fields_are_rejected() {
    let test = InterceptionTest::new();
    let fetch = test.fetch(test.request().build());
    let (request, sender) = test.next_request();
    let mut response_headers = headers(&[("content-length", "1")]);
    response_headers.append(header::CONTENT_LENGTH, HeaderValue::from_static("2"));
    send(
        &sender,
        WebResourceResponseMsg::Start(
            WebResourceResponse::new(request.url).headers(response_headers),
        ),
    );

    assert_resource_error(&fetch.finish());
    test.assert_no_network();
}

#[test]
fn content_length_with_transfer_encoding_is_rejected() {
    let test = InterceptionTest::new();
    let fetch = test.fetch(test.request().build());
    let (request, sender) = test.next_request();
    send(
        &sender,
        WebResourceResponseMsg::Start(WebResourceResponse::new(request.url).headers(headers(&[
            ("content-length", "1"),
            ("transfer-encoding", "chunked"),
        ]))),
    );

    assert_resource_error(&fetch.finish());
    test.assert_no_network();
}

#[test]
fn matching_content_length_fields_accept_the_complete_body() {
    let test = InterceptionTest::new();
    for duplicate in [false, true] {
        let fetch = test.fetch(test.request().build());
        let (request, sender) = test.next_request();
        let mut response_headers = headers(&[("content-length", "3")]);
        if duplicate {
            response_headers.append(header::CONTENT_LENGTH, HeaderValue::from_static("3"));
        }
        respond(
            &sender,
            WebResourceResponse::new(request.url).headers(response_headers),
            &[b"a", b"bc"],
        );

        assert_body(&fetch.finish(), b"abc");
    }
    test.assert_no_network();
}

#[test]
fn completed_body_must_match_declared_content_length() {
    let test = InterceptionTest::new();
    for (length, body) in [
        ("3", b"a".as_slice()),
        ("1", b"abc".as_slice()),
        ("0", b"a".as_slice()),
        ("1", b"".as_slice()),
    ] {
        let fetch = test.fetch(test.request().build());
        let (request, sender) = test.next_request();
        send(
            &sender,
            WebResourceResponseMsg::Start(
                WebResourceResponse::new(request.url)
                    .headers(headers(&[("content-length", length)])),
            ),
        );
        send(&sender, WebResourceResponseMsg::SendBodyData(body.to_vec()));
        // An oversized body can be rejected as soon as its data arrives, before this send.
        let _ = sender.send(WebResourceResponseMsg::FinishLoad);

        assert_resource_error(&fetch.finish());
    }
    test.assert_no_network();
}

#[test]
fn head_and_not_modified_allow_large_representation_lengths_without_body() {
    let test = InterceptionTest::new();
    let representation_length =
        (servo_config::pref!(network_interception_max_response_bytes) + 1).to_string();
    for (method, status) in [
        (Method::HEAD, StatusCode::OK),
        (Method::GET, StatusCode::NOT_MODIFIED),
    ] {
        let fetch = test.fetch(test.request().method(method).build());
        let (request, sender) = test.next_request();
        respond(
            &sender,
            WebResourceResponse::new(request.url)
                .status_code(status)
                .headers(headers(&[("content-length", &representation_length)])),
            &[],
        );

        let response = fetch.finish();
        assert_eq!(response.status.code(), status);
        assert_eq!(
            response.headers[header::CONTENT_LENGTH],
            representation_length.as_str()
        );
        assert_no_body(&response);
    }
    test.assert_no_network();
}

#[test]
fn representation_only_responses_still_reject_invalid_content_length() {
    let test = InterceptionTest::new();
    for (method, status) in [
        (Method::HEAD, StatusCode::OK),
        (Method::GET, StatusCode::NOT_MODIFIED),
    ] {
        let fetch = test.fetch(test.request().method(method).build());
        let (request, sender) = test.next_request();
        send(
            &sender,
            WebResourceResponseMsg::Start(
                WebResourceResponse::new(request.url)
                    .status_code(status)
                    .headers(headers(&[("content-length", "invalid")])),
            ),
        );

        assert_resource_error(&fetch.finish());
    }
    test.assert_no_network();
}

#[test]
fn null_body_statuses_accept_empty_completion() {
    let test = InterceptionTest::new();
    for status in [
        StatusCode::NO_CONTENT,
        StatusCode::RESET_CONTENT,
        StatusCode::NOT_MODIFIED,
    ] {
        let fetch = test.fetch(test.request().build());
        let (request, sender) = test.next_request();
        respond(
            &sender,
            WebResourceResponse::new(request.url).status_code(status),
            &[],
        );

        let response = fetch.finish();
        assert_eq!(response.status.code(), status);
        assert_no_body(&response);
    }
    test.assert_no_network();
}

#[test]
fn no_content_and_reset_content_reject_nonzero_declared_length() {
    let test = InterceptionTest::new();
    for status in [StatusCode::NO_CONTENT, StatusCode::RESET_CONTENT] {
        let fetch = test.fetch(test.request().build());
        let (request, sender) = test.next_request();
        send(
            &sender,
            WebResourceResponseMsg::Start(
                WebResourceResponse::new(request.url)
                    .status_code(status)
                    .headers(headers(&[("content-length", "1")])),
            ),
        );

        assert_resource_error(&fetch.finish());
    }
    test.assert_no_network();
}

#[test]
fn head_and_null_body_statuses_reject_payload_bytes() {
    let test = InterceptionTest::new();
    for (method, status) in [
        (Method::HEAD, StatusCode::OK),
        (Method::GET, StatusCode::NO_CONTENT),
        (Method::GET, StatusCode::RESET_CONTENT),
        (Method::GET, StatusCode::NOT_MODIFIED),
    ] {
        let fetch = test.fetch(test.request().method(method).build());
        let (request, sender) = test.next_request();
        send(
            &sender,
            WebResourceResponseMsg::Start(
                WebResourceResponse::new(request.url).status_code(status),
            ),
        );
        send(&sender, WebResourceResponseMsg::SendBodyData(vec![b'x']));
        let _ = sender.send(WebResourceResponseMsg::FinishLoad);

        assert_resource_error(&fetch.finish());
    }
    test.assert_no_network();
}

#[test]
fn informational_status_cannot_be_the_final_intercepted_response() {
    let test = InterceptionTest::new();
    for status in [100, 101, 102, 103, 199] {
        let fetch = test.fetch(test.request().build());
        let (request, sender) = test.next_request();
        send(
            &sender,
            WebResourceResponseMsg::Start(
                WebResourceResponse::new(request.url)
                    .status_code(StatusCode::from_u16(status).unwrap()),
            ),
        );
        let _ = sender.send(WebResourceResponseMsg::FinishLoad);

        assert_resource_error(&fetch.finish());
    }
    test.assert_no_network();
}

#[test]
fn finish_and_cancellation_race_completes_boundedly() {
    let test = InterceptionTest::new();
    let body = vec![b'x'; 4096];
    // Exercise both event orders and the handoff after the interceptor closes its channel.
    // This is scheduling stress, not deterministic proof of a particular decoded-stream race.
    for attempt in 0..96 {
        let fetch = test.fetch(test.request().build());
        let (request, sender) = test.next_request();
        send(
            &sender,
            WebResourceResponseMsg::Start(WebResourceResponse::new(request.url)),
        );
        send(&sender, WebResourceResponseMsg::SendBodyData(body.clone()));

        let cancellation_listener = fetch.cancellation_listener.clone();
        let (release, wait_for_release) = bounded(1);
        let (cancelled, cancellation_complete) = bounded(1);
        std::thread::spawn(move || {
            wait_for_release
                .recv_timeout(TEST_TIMEOUT)
                .expect("race must release cancellation");
            cancellation_listener.cancel();
            let _ = cancelled.send(());
        });

        if attempt % 3 == 0 {
            release.send(()).unwrap();
            // Cancellation may already have closed the response channel.
            let _ = sender.send(WebResourceResponseMsg::FinishLoad);
        } else {
            send(&sender, WebResourceResponseMsg::FinishLoad);
            if attempt % 3 == 2 {
                spawn_blocking_task::<_, ()>(async {
                    tokio::time::timeout(TEST_TIMEOUT, sender.closed())
                        .await
                        .expect("completed interceptor must close its channel");
                });
            }
            release.send(()).unwrap();
        }
        cancellation_complete
            .recv_timeout(TEST_TIMEOUT)
            .expect("cancellation must complete");

        let response = fetch.finish();
        match response.get_network_error() {
            Some(NetworkError::LoadCancelled) => {},
            Some(error) => panic!("unexpected failure in finish/cancel race: {error:?}"),
            None if response.aborted.load(Ordering::Acquire) => assert_no_body(&response),
            None => assert_body(&response, &body),
        }
    }
    test.assert_no_more_requests();
    test.assert_no_network();
}
