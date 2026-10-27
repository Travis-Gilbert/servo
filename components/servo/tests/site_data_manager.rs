/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

mod common;

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use cookie::Cookie;
use http::HeaderValue;
use http_body_util::combinators::BoxBody;
use hyper::body::{Bytes, Incoming};
use hyper::{Request as HyperRequest, Response as HyperResponse};
use net::test_util::{Server, make_body, make_server, replace_host_table};
use net_traits::CookieSource;
use net_traits::blob_url_store::UrlWithBlobClaim;
use servo::{JSValue, Servo, ServoUrl, SiteData, StorageType, WebView, WebViewBuilder};

use crate::common::{ServoTest, WebViewDelegateImpl, evaluate_javascript};

pub struct WebViewTest {
    servo_test: ServoTest,
    delegate: Rc<WebViewDelegateImpl>,
    webview: WebView,
}

impl WebViewTest {
    fn new() -> Self {
        let servo_test = ServoTest::new();
        let delegate = Rc::new(WebViewDelegateImpl::default());
        let webview = WebViewBuilder::new(servo_test.servo(), servo_test.rendering_context.clone())
            .delegate(delegate.clone())
            .build();
        let delegate_clone = delegate.clone();
        servo_test.spin(move || !delegate_clone.url_changed.get());

        Self {
            servo_test,
            delegate,
            webview,
        }
    }

    fn servo(&self) -> &Servo {
        &self.servo_test.servo()
    }

    fn load_and_wait(&self, url: ServoUrl) {
        self.delegate.reset();
        self.webview.load(url.clone().into_url());
        let delegate_clone = self.delegate.clone();
        self.servo_test
            .spin(move || !delegate_clone.url_changed.get());
    }

    fn evaluate_javascript(&self, script: impl ToString) {
        let _ = evaluate_javascript(&self.servo_test, self.webview.clone(), script);
    }
}

// Note: MSRV currently rejects a borrowed callback here. Can be switched back
// to a borrowed callback once MSRV is eventually bumped, so this can become:
// `type TestSiteDataStep<'a> = (SiteData, Option<&'a dyn Fn(&SiteData)>);`
type TestSiteDataStep<'a> = (SiteData, Option<Box<dyn Fn(&SiteData) + 'a>>);

fn run_test_site_data_steps(webview_test: &WebViewTest, steps: &[TestSiteDataStep]) {
    let ip = "127.0.0.1".parse().unwrap();
    let host_table: HashMap<_, _> = steps
        .iter()
        .map(|step| (format!("www.{}", step.0.name()), ip))
        .collect();
    replace_host_table(host_table);

    static MESSAGE: &'static [u8] = b"<!DOCTYPE html>\nHello";
    let handler =
        move |_: HyperRequest<Incoming>,
              response: &mut HyperResponse<BoxBody<Bytes, hyper::Error>>| {
            *response.body_mut() = make_body(MESSAGE.to_vec());
        };

    let mut servers: Vec<(Server, UrlWithBlobClaim)> =
        (0..steps.len()).map(|_| make_server(handler)).collect();
    servers.sort_by(|(_, a), (_, b)| a.cmp(b));

    for ((site, callback), (server, url)) in steps.iter().zip(servers.into_iter()) {
        let port = url.port().unwrap();
        let custom_url = ServoUrl::parse(&format!("http://www.{}:{port}", site.name())).unwrap();

        webview_test.load_and_wait(custom_url.clone());

        let _ = server.close();

        let storage_types = site.storage_types();

        if storage_types.contains(StorageType::Cookies) {
            webview_test.evaluate_javascript("document.cookie = 'foo=bar';");
        }

        if storage_types.contains(StorageType::Local) {
            webview_test.evaluate_javascript("localStorage.setItem('foo', 'bar');");
        }

        if storage_types.contains(StorageType::Session) {
            webview_test.evaluate_javascript("sessionStorage.setItem('foo', 'bar');");
        }

        if let Some(callback) = callback {
            callback(site);
        }
    }
}

#[test]
fn test_site_data() {
    let webview_test = WebViewTest::new();

    let site_data_manager = webview_test.servo().site_data_manager();

    let sites = site_data_manager.site_data(StorageType::Cookies);
    assert_eq!(sites.len(), 0);
    let sites = site_data_manager.site_data(StorageType::Local);
    assert_eq!(sites.len(), 0);
    let sites = site_data_manager.site_data(StorageType::Session);
    assert_eq!(sites.len(), 0);
    let sites = site_data_manager.site_data(StorageType::all());
    assert_eq!(sites.len(), 0);

    let steps: &[TestSiteDataStep] = &[
        (
            SiteData::new("site-data-0.test", StorageType::Cookies),
            Some(Box::new(|_| {
                let sites = site_data_manager.site_data(StorageType::Cookies);
                assert_eq!(
                    &sites,
                    &[SiteData::new("site-data-0.test", StorageType::Cookies),]
                );
                let sites = site_data_manager.site_data(StorageType::Local);
                assert_eq!(sites.len(), 0);
                let sites = site_data_manager.site_data(StorageType::Session);
                assert_eq!(sites.len(), 0);
                let sites = site_data_manager.site_data(StorageType::all());
                assert_eq!(
                    &sites,
                    &[SiteData::new("site-data-0.test", StorageType::Cookies),]
                );
            })),
        ),
        (
            SiteData::new("site-data-1.test", StorageType::Local),
            Some(Box::new(|_| {
                let sites = site_data_manager.site_data(StorageType::Cookies);
                assert_eq!(
                    &sites,
                    &[SiteData::new("site-data-0.test", StorageType::Cookies),]
                );
                let sites = site_data_manager.site_data(StorageType::Local);
                assert_eq!(
                    &sites,
                    &[SiteData::new("site-data-1.test", StorageType::Local),]
                );
                let sites = site_data_manager.site_data(StorageType::Session);
                assert_eq!(sites.len(), 0);
                let sites = site_data_manager.site_data(StorageType::all());
                assert_eq!(
                    &sites,
                    &[
                        SiteData::new("site-data-0.test", StorageType::Cookies),
                        SiteData::new("site-data-1.test", StorageType::Local),
                    ]
                );
            })),
        ),
        (
            SiteData::new("site-data-2.test", StorageType::Session),
            Some(Box::new(|_| {
                let sites = site_data_manager.site_data(StorageType::Cookies);
                assert_eq!(
                    &sites,
                    &[SiteData::new("site-data-0.test", StorageType::Cookies),]
                );
                let sites = site_data_manager.site_data(StorageType::Local);
                assert_eq!(
                    &sites,
                    &[SiteData::new("site-data-1.test", StorageType::Local),]
                );
                let sites = site_data_manager.site_data(StorageType::Session);
                assert_eq!(
                    &sites,
                    &[SiteData::new("site-data-2.test", StorageType::Session),]
                );
                let sites = site_data_manager.site_data(StorageType::all());
                assert_eq!(
                    &sites,
                    &[
                        SiteData::new("site-data-0.test", StorageType::Cookies),
                        SiteData::new("site-data-1.test", StorageType::Local),
                        SiteData::new("site-data-2.test", StorageType::Session),
                    ]
                );
            })),
        ),
        (
            SiteData::new(
                "site-data-3.test",
                StorageType::Cookies | StorageType::Local | StorageType::Session,
            ),
            Some(Box::new(|_| {
                let sites = site_data_manager.site_data(StorageType::Cookies);
                assert_eq!(
                    &sites,
                    &[
                        SiteData::new("site-data-0.test", StorageType::Cookies),
                        SiteData::new("site-data-3.test", StorageType::Cookies),
                    ]
                );
                let sites = site_data_manager.site_data(StorageType::Local);
                assert_eq!(
                    &sites,
                    &[
                        SiteData::new("site-data-1.test", StorageType::Local),
                        SiteData::new("site-data-3.test", StorageType::Local),
                    ]
                );
                let sites = site_data_manager.site_data(StorageType::Session);
                assert_eq!(
                    &sites,
                    &[
                        SiteData::new("site-data-2.test", StorageType::Session),
                        SiteData::new("site-data-3.test", StorageType::Session),
                    ]
                );
                let sites = site_data_manager.site_data(StorageType::all());
                assert_eq!(
                    &sites,
                    &[
                        SiteData::new("site-data-0.test", StorageType::Cookies),
                        SiteData::new("site-data-1.test", StorageType::Local),
                        SiteData::new("site-data-2.test", StorageType::Session),
                        SiteData::new(
                            "site-data-3.test",
                            StorageType::Cookies | StorageType::Local | StorageType::Session
                        ),
                    ]
                );
            })),
        ),
    ];

    run_test_site_data_steps(&webview_test, steps);
}

#[test]
fn test_clear_site_data_cookies() {
    let webview_test = WebViewTest::new();

    let site_data_manager = webview_test.servo().site_data_manager();

    let sites = site_data_manager.site_data(StorageType::all());
    assert_eq!(sites.len(), 0);

    let steps: &[TestSiteDataStep] = &[
        (
            SiteData::new("site-data-0a.test", StorageType::Cookies),
            None,
        ),
        (
            SiteData::new("site-data-0b.test", StorageType::Cookies),
            None,
        ),
        (
            SiteData::new("site-data-0c.test", StorageType::Cookies),
            None,
        ),
        (SiteData::new("site-data-1.test", StorageType::Local), None),
        (
            SiteData::new("site-data-2.test", StorageType::Session),
            None,
        ),
        (
            SiteData::new(
                "site-data-3.test",
                StorageType::Cookies | StorageType::Local | StorageType::Session,
            ),
            None,
        ),
    ];

    run_test_site_data_steps(&webview_test, steps);

    let sites = site_data_manager.site_data(StorageType::all());
    assert_eq!(
        &sites,
        &[
            SiteData::new("site-data-0a.test", StorageType::Cookies),
            SiteData::new("site-data-0b.test", StorageType::Cookies),
            SiteData::new("site-data-0c.test", StorageType::Cookies),
            SiteData::new("site-data-1.test", StorageType::Local),
            SiteData::new("site-data-2.test", StorageType::Session),
            SiteData::new(
                "site-data-3.test",
                StorageType::Cookies | StorageType::Local | StorageType::Session
            ),
        ]
    );

    site_data_manager.clear_site_data(&["site-data-0.test"], StorageType::Cookies);

    let sites = site_data_manager.site_data(StorageType::all());
    assert_eq!(
        &sites,
        &[
            SiteData::new("site-data-0a.test", StorageType::Cookies),
            SiteData::new("site-data-0b.test", StorageType::Cookies),
            SiteData::new("site-data-0c.test", StorageType::Cookies),
            SiteData::new("site-data-1.test", StorageType::Local),
            SiteData::new("site-data-2.test", StorageType::Session),
            SiteData::new(
                "site-data-3.test",
                StorageType::Cookies | StorageType::Local | StorageType::Session
            ),
        ]
    );

    site_data_manager.clear_site_data(&["site-data-0a.test"], StorageType::Cookies);

    let sites = site_data_manager.site_data(StorageType::all());
    assert_eq!(
        &sites,
        &[
            SiteData::new("site-data-0b.test", StorageType::Cookies),
            SiteData::new("site-data-0c.test", StorageType::Cookies),
            SiteData::new("site-data-1.test", StorageType::Local),
            SiteData::new("site-data-2.test", StorageType::Session),
            SiteData::new(
                "site-data-3.test",
                StorageType::Cookies | StorageType::Local | StorageType::Session
            ),
        ]
    );

    site_data_manager.clear_site_data(
        &["site-data-0c.test", "site-data-3.test"],
        StorageType::Cookies,
    );

    let sites = site_data_manager.site_data(StorageType::all());
    assert_eq!(
        &sites,
        &[
            SiteData::new("site-data-0b.test", StorageType::Cookies),
            SiteData::new("site-data-1.test", StorageType::Local),
            SiteData::new("site-data-2.test", StorageType::Session),
            SiteData::new(
                "site-data-3.test",
                StorageType::Local | StorageType::Session
            ),
        ]
    );
}

#[test]
fn test_clear_site_data_local() {
    let webview_test = WebViewTest::new();

    let site_data_manager = webview_test.servo().site_data_manager();

    let sites = site_data_manager.site_data(StorageType::all());
    assert_eq!(sites.len(), 0);

    let steps: &[TestSiteDataStep] = &[
        (
            SiteData::new("site-data-0.test", StorageType::Cookies),
            None,
        ),
        (SiteData::new("site-data-1a.test", StorageType::Local), None),
        (SiteData::new("site-data-1b.test", StorageType::Local), None),
        (SiteData::new("site-data-1c.test", StorageType::Local), None),
        (
            SiteData::new("site-data-2.test", StorageType::Session),
            None,
        ),
        (
            SiteData::new(
                "site-data-3.test",
                StorageType::Cookies | StorageType::Local | StorageType::Session,
            ),
            None,
        ),
    ];

    run_test_site_data_steps(&webview_test, steps);

    let sites = site_data_manager.site_data(StorageType::all());
    assert_eq!(
        &sites,
        &[
            SiteData::new("site-data-0.test", StorageType::Cookies),
            SiteData::new("site-data-1a.test", StorageType::Local),
            SiteData::new("site-data-1b.test", StorageType::Local),
            SiteData::new("site-data-1c.test", StorageType::Local),
            SiteData::new("site-data-2.test", StorageType::Session),
            SiteData::new(
                "site-data-3.test",
                StorageType::Cookies | StorageType::Local | StorageType::Session
            ),
        ]
    );

    site_data_manager.clear_site_data(&["site-data-1.test"], StorageType::Local);

    let sites = site_data_manager.site_data(StorageType::all());
    assert_eq!(
        &sites,
        &[
            SiteData::new("site-data-0.test", StorageType::Cookies),
            SiteData::new("site-data-1a.test", StorageType::Local),
            SiteData::new("site-data-1b.test", StorageType::Local),
            SiteData::new("site-data-1c.test", StorageType::Local),
            SiteData::new("site-data-2.test", StorageType::Session),
            SiteData::new(
                "site-data-3.test",
                StorageType::Cookies | StorageType::Local | StorageType::Session
            ),
        ]
    );

    site_data_manager.clear_site_data(&["site-data-1a.test"], StorageType::Local);

    let sites = site_data_manager.site_data(StorageType::all());
    assert_eq!(
        &sites,
        &[
            SiteData::new("site-data-0.test", StorageType::Cookies),
            SiteData::new("site-data-1b.test", StorageType::Local),
            SiteData::new("site-data-1c.test", StorageType::Local),
            SiteData::new("site-data-2.test", StorageType::Session),
            SiteData::new(
                "site-data-3.test",
                StorageType::Cookies | StorageType::Local | StorageType::Session
            ),
        ]
    );

    site_data_manager.clear_site_data(
        &["site-data-1c.test", "site-data-3.test"],
        StorageType::Local,
    );

    let sites = site_data_manager.site_data(StorageType::all());
    assert_eq!(
        &sites,
        &[
            SiteData::new("site-data-0.test", StorageType::Cookies),
            SiteData::new("site-data-1b.test", StorageType::Local),
            SiteData::new("site-data-2.test", StorageType::Session),
            SiteData::new(
                "site-data-3.test",
                StorageType::Cookies | StorageType::Session
            ),
        ]
    );
}

#[test]
fn test_clear_site_data_session() {
    let webview_test = WebViewTest::new();

    let site_data_manager = webview_test.servo().site_data_manager();

    let sites = site_data_manager.site_data(StorageType::all());
    assert_eq!(sites.len(), 0);

    let steps: &[TestSiteDataStep] = &[
        (
            SiteData::new("site-data-0.test", StorageType::Cookies),
            None,
        ),
        (SiteData::new("site-data-1.test", StorageType::Local), None),
        (
            SiteData::new("site-data-2a.test", StorageType::Session),
            None,
        ),
        (
            SiteData::new("site-data-2b.test", StorageType::Session),
            None,
        ),
        (
            SiteData::new("site-data-2c.test", StorageType::Session),
            None,
        ),
        (
            SiteData::new(
                "site-data-3.test",
                StorageType::Cookies | StorageType::Local | StorageType::Session,
            ),
            None,
        ),
    ];

    run_test_site_data_steps(&webview_test, steps);

    let sites = site_data_manager.site_data(StorageType::all());
    assert_eq!(
        &sites,
        &[
            SiteData::new("site-data-0.test", StorageType::Cookies),
            SiteData::new("site-data-1.test", StorageType::Local),
            SiteData::new("site-data-2a.test", StorageType::Session),
            SiteData::new("site-data-2b.test", StorageType::Session),
            SiteData::new("site-data-2c.test", StorageType::Session),
            SiteData::new(
                "site-data-3.test",
                StorageType::Cookies | StorageType::Local | StorageType::Session
            ),
        ]
    );

    site_data_manager.clear_site_data(&["site-data-2.test"], StorageType::Session);

    let sites = site_data_manager.site_data(StorageType::all());
    assert_eq!(
        &sites,
        &[
            SiteData::new("site-data-0.test", StorageType::Cookies),
            SiteData::new("site-data-1.test", StorageType::Local),
            SiteData::new("site-data-2a.test", StorageType::Session),
            SiteData::new("site-data-2b.test", StorageType::Session),
            SiteData::new("site-data-2c.test", StorageType::Session),
            SiteData::new(
                "site-data-3.test",
                StorageType::Cookies | StorageType::Local | StorageType::Session
            ),
        ]
    );

    site_data_manager.clear_site_data(&["site-data-2a.test"], StorageType::Session);

    let sites = site_data_manager.site_data(StorageType::all());
    assert_eq!(
        &sites,
        &[
            SiteData::new("site-data-0.test", StorageType::Cookies),
            SiteData::new("site-data-1.test", StorageType::Local),
            SiteData::new("site-data-2b.test", StorageType::Session),
            SiteData::new("site-data-2c.test", StorageType::Session),
            SiteData::new(
                "site-data-3.test",
                StorageType::Cookies | StorageType::Local | StorageType::Session
            ),
        ]
    );

    site_data_manager.clear_site_data(
        &["site-data-2c.test", "site-data-3.test"],
        StorageType::Session,
    );

    let sites = site_data_manager.site_data(StorageType::all());
    assert_eq!(
        &sites,
        &[
            SiteData::new("site-data-0.test", StorageType::Cookies),
            SiteData::new("site-data-1.test", StorageType::Local),
            SiteData::new("site-data-2b.test", StorageType::Session),
            SiteData::new(
                "site-data-3.test",
                StorageType::Cookies | StorageType::Local
            ),
        ]
    );
}

#[test]
fn test_clear_cookies() {
    let servo_test = ServoTest::new();

    static MESSAGE: &'static [u8] = b"<!DOCTYPE html>\nHello";

    let handler =
        move |_: HyperRequest<Incoming>,
              response: &mut HyperResponse<BoxBody<Bytes, hyper::Error>>| {
            *response.body_mut() = make_body(MESSAGE.to_vec());
        };
    let (server, url) = make_server(handler);

    let delegate = Rc::new(WebViewDelegateImpl::default());

    let webview = WebViewBuilder::new(servo_test.servo(), servo_test.rendering_context.clone())
        .delegate(delegate.clone())
        .url(url.as_url().clone())
        .build();

    servo_test.spin(move || !delegate.url_changed.get());

    let _ = server.close();

    let result = evaluate_javascript(
        &servo_test,
        webview.clone(),
        "document.cookie = 'foo1=bar1';\
         document.cookie = 'foo2=bar2';\
         document.cookie = 'foo3=bar3';\
         document.cookie;",
    );
    assert_eq!(
        result,
        Ok(JSValue::String("foo1=bar1; foo2=bar2; foo3=bar3".into()))
    );

    servo_test.servo().site_data_manager().clear_cookies(None);

    let result = evaluate_javascript(&servo_test, webview.clone(), "document.cookie");
    assert_eq!(result, Ok(JSValue::String(String::new())));
}

#[test]
fn test_get_cookie() {
    let servo_test = ServoTest::new();

    // Serve a minimal page that sets a cookie via Set-Cookie response header.
    let handler =
        move |_: HyperRequest<Incoming>,
              response: &mut HyperResponse<BoxBody<Bytes, hyper::Error>>| {
            response.headers_mut().insert(
                http::header::SET_COOKIE,
                HeaderValue::from_static("foo=bar; Path=/"),
            );
            *response.body_mut() = make_body(b"<!DOCTYPE html><p>hi</p>".to_vec());
        };
    let (server, url) = make_server(handler);

    let delegate = Rc::new(WebViewDelegateImpl::default());
    let _webview = WebViewBuilder::new(servo_test.servo(), servo_test.rendering_context.clone())
        .delegate(delegate.clone())
        .url(url.url().into_url())
        .build();

    // Wait for LoadStatus::Complete to ensure the HTTP response and Set-Cookie header are processed.
    servo_test.spin(move || !delegate.load_status_changed.get());
    let _ = server.close();

    let cookies = servo_test
        .servo()
        .site_data_manager()
        .cookies_for_url(url.url().into_url(), CookieSource::NonHTTP);
    assert_eq!(cookies.len(), 1);
    assert_eq!(cookies[0].name(), "foo");
    assert_eq!(cookies[0].value(), "bar");
}

#[test]
fn test_set_cookie() {
    let servo_test = ServoTest::new();

    let received_cookie: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let received_cookie_clone = received_cookie.clone();

    // Serve a minimal page; on the second load, capture the Cookie request header.
    let handler =
        move |req: HyperRequest<Incoming>,
              response: &mut HyperResponse<BoxBody<Bytes, hyper::Error>>| {
            if let Some(cookie) = req.headers().get(http::header::COOKIE) {
                *received_cookie_clone.lock().unwrap() = Some(cookie.to_str().unwrap().to_string());
            }
            *response.body_mut() = make_body(b"<!DOCTYPE html><p>hi</p>".to_vec());
        };
    let (server, url) = make_server(handler);
    let page_url = url.url().into_url();

    let delegate = Rc::new(WebViewDelegateImpl::default());
    let delegate_clone = delegate.clone();
    let webview = WebViewBuilder::new(servo_test.servo(), servo_test.rendering_context.clone())
        .delegate(delegate.clone())
        .url(page_url.clone())
        .build();

    servo_test.spin(move || !delegate.load_status_changed.get());

    // Set a cookie via the site data manager.
    let cookie = Cookie::build(("foo", "bar")).path("/").build();
    servo_test
        .servo()
        .site_data_manager()
        .set_cookie_for_url(page_url.clone(), cookie, None);

    // Verify it is returned by get_cookies_for_url.
    // Don't need sync call because set and get messages are processed in order.
    let cookies = servo_test
        .servo()
        .site_data_manager()
        .cookies_for_url(page_url.clone(), CookieSource::HTTP);
    assert_eq!(cookies.len(), 1);
    assert_eq!(cookies[0].name(), "foo");
    assert_eq!(cookies[0].value(), "bar");

    // Load the page again and verify the cookie is sent in the request.
    delegate_clone.reset();
    delegate_clone.load_status_changed.set(false);
    webview.load(page_url.into());
    let delegate_clone2 = delegate_clone.clone();
    servo_test.spin(move || !delegate_clone2.load_status_changed.get());

    let _ = server.close();

    assert_eq!(
        *received_cookie.lock().unwrap(),
        Some("foo=bar".to_string())
    );
}

#[test]
fn test_get_cookie_async() {
    let servo_test = ServoTest::new();

    // Serve a minimal page that sets a cookie via Set-Cookie response header.
    let handler =
        move |_: HyperRequest<Incoming>,
              response: &mut HyperResponse<BoxBody<Bytes, hyper::Error>>| {
            response.headers_mut().insert(
                http::header::SET_COOKIE,
                HeaderValue::from_static("foo=bar; Path=/"),
            );
            *response.body_mut() = make_body(b"<!DOCTYPE html><p>hi</p>".to_vec());
        };
    let (server, url) = make_server(handler);

    let delegate = Rc::new(WebViewDelegateImpl::default());
    let _webview = WebViewBuilder::new(servo_test.servo(), servo_test.rendering_context.clone())
        .delegate(delegate.clone())
        .url(url.url().into_url())
        .build();

    // Wait for LoadStatus::Complete to ensure the HTTP response and Set-Cookie header are processed.
    servo_test.spin(move || !delegate.load_status_changed.get());
    let _ = server.close();

    // Issue an async cookie request and verify it does not block.
    let result: Rc<RefCell<Option<Vec<Cookie<'static>>>>> = Rc::new(RefCell::new(None));
    let continued_after_call = Rc::new(Cell::new(false));
    let continued_clone = continued_after_call.clone();
    let result_clone = result.clone();
    servo_test
        .servo()
        .site_data_manager()
        .cookies_for_url_async(
            url.as_url().clone(),
            CookieSource::NonHTTP,
            move |cookies| {
                assert!(continued_clone.get(), "callback fired synchronously");
                *result_clone.borrow_mut() = Some(cookies);
            },
        );
    assert!(result.borrow().is_none(), "result available before spin");
    continued_after_call.set(true);

    // Spin the event loop until the callback fires.
    let result_clone = result.clone();
    servo_test.spin(move || result_clone.borrow().is_none());

    let cookies = result.borrow();
    let cookies = cookies.as_ref().unwrap();
    assert_eq!(cookies.len(), 1);
    assert_eq!(cookies[0].name(), "foo");
    assert_eq!(cookies[0].value(), "bar");
}

#[test]
fn test_partition_roots_and_interleaved_management_callbacks() {
    let root = tempfile::tempdir().unwrap();
    let test = ServoTest::new();
    let a = test
        .servo()
        .create_storage_partition(root.path().join("A"))
        .unwrap();
    let b = test
        .servo()
        .create_storage_partition(root.path().join("B"))
        .unwrap();
    assert_ne!(a.id(), b.id());
    assert_ne!(a.root(), b.root());
    assert!(matches!(
        test.servo()
            .create_storage_partition(root.path().join("A/../A")),
        Err(servo::StoragePartitionError::DuplicateRoot(_))
    ));
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(root.path().join("A"), root.path().join("alias")).unwrap();
        assert!(matches!(
            test.servo()
                .create_storage_partition(root.path().join("alias")),
            Err(servo::StoragePartitionError::DuplicateRoot(_))
        ));
    }
    let url = url::Url::parse("https://profiles.example.com/").unwrap();
    let completions = Rc::new(Cell::new(0));
    for (partition, value) in [(&a, "A"), (&b, "B")] {
        let completed = completions.clone();
        partition.site_data_manager().set_cookie_for_url(
            url.clone(),
            Cookie::new("profile", value),
            Some(Box::new(move || completed.set(completed.get() + 1))),
        );
    }
    let results = Rc::new(RefCell::new(Vec::new()));
    for (partition, value) in [(&b, "B"), (&a, "A")] {
        let results = results.clone();
        partition.site_data_manager().cookies_for_url_async(
            url.clone(),
            CookieSource::HTTP,
            move |cookies| {
                assert_eq!(cookies.len(), 1);
                assert_eq!(cookies[0].value(), value);
                results.borrow_mut().push(value);
            },
        );
    }
    test.spin(|| completions.get() != 2 || results.borrow().len() != 2);
    a.site_data_manager().clear_cookies(None);
    assert!(
        a.site_data_manager()
            .cookies_for_url(url.clone(), CookieSource::HTTP)
            .is_empty()
    );
    assert_eq!(
        b.site_data_manager()
            .cookies_for_url(url, CookieSource::HTTP)[0]
            .value(),
        "B"
    );
    a.network_manager().clear_cache();

    // Ephemeral owner creation and shutdown must not rewrite registered profiles.
    let before = std::fs::read(root.path().join("A/clientstorage/default_v1/reg.sqlite")).unwrap();
    let ephemeral = test.servo().create_ephemeral_storage_partition().unwrap();
    assert!(ephemeral.root().is_none());
    ephemeral.site_data_manager().set_cookie_for_url(
        url::Url::parse("https://profiles.example.com/").unwrap(),
        Cookie::new("profile", "temporary"),
        None,
    );
    assert_eq!(
        before,
        std::fs::read(root.path().join("A/clientstorage/default_v1/reg.sqlite")).unwrap()
    );
    drop(test);
    assert!(root.path().join("A/cookie_jar.json").exists());
    assert!(root.path().join("B/cookie_jar.json").exists());
}

#[test]
fn test_partition_initialization_errors_do_not_select_default() {
    let root = tempfile::tempdir().unwrap();
    let test = ServoTest::new_with_builder(|builder| {
        let mut preferences = servo::Preferences::default();
        preferences.network_http_disk_cache = "enabled-for-partitions".into();
        builder.preferences(preferences)
    });
    let blocked = root.path().join("blocked");
    std::fs::write(&blocked, b"not a directory").unwrap();
    assert!(matches!(
        test.servo().create_storage_partition(&blocked),
        Err(servo::StoragePartitionError::Io(_))
    ));
    let bad_cache = root.path().join("bad-cache");
    std::fs::create_dir_all(bad_cache.join("http-cache/cache.sqlite3")).unwrap();
    assert!(matches!(
        test.servo().create_storage_partition(&bad_cache),
        Err(servo::StoragePartitionError::Registration(_))
    ));
    let good = test
        .servo()
        .create_storage_partition(root.path().join("good"))
        .unwrap();
    assert_ne!(good.id(), test.servo().default_storage_partition().id());
    assert!(
        good.root()
            .unwrap()
            .join("http-cache/cache.sqlite3")
            .is_file()
    );
    assert!(
        test.servo()
            .create_ephemeral_storage_partition()
            .unwrap()
            .root()
            .is_none()
    );
}

#[test]
fn test_partition_cache_and_site_data_clears_leave_other_profiles_intact() {
    let test = ServoTest::new();
    let (server, url) = make_server(|_, response| {
        response.headers_mut().insert(
            http::header::CACHE_CONTROL,
            HeaderValue::from_static("public, max-age=3600"),
        );
        *response.body_mut() = make_body(b"<!doctype html>Partition management".to_vec());
    });
    let partitions = [
        test.servo().create_ephemeral_storage_partition().unwrap(),
        test.servo().create_ephemeral_storage_partition().unwrap(),
    ];
    let views = partitions.each_ref().map(|partition| {
        WebViewBuilder::new(test.servo(), test.rendering_context.clone())
            .storage_partition(partition.clone())
            .url(url::Url::parse(url.as_str()).unwrap())
            .build()
    });
    for (index, view) in views.iter().enumerate() {
        evaluate_javascript(
            &test,
            view.clone(),
            format!("localStorage.profile = '{index}'; sessionStorage.profile = '{index}';"),
        )
        .unwrap();
        assert!(
            !partitions[index]
                .network_manager()
                .cache_entries()
                .is_empty()
        );
    }
    partitions[0].network_manager().clear_cache();
    assert!(partitions[0].network_manager().cache_entries().is_empty());
    assert!(!partitions[1].network_manager().cache_entries().is_empty());
    partitions[0]
        .site_data_manager()
        .clear_site_data(&["localhost"], StorageType::Local | StorageType::Session);
    assert_eq!(
        evaluate_javascript(
            &test,
            views[0].clone(),
            "localStorage.getItem('profile') === null && sessionStorage.getItem('profile') === null"
        )
        .unwrap(),
        JSValue::Boolean(true)
    );
    assert_eq!(
        evaluate_javascript(
            &test,
            views[1].clone(),
            "localStorage.profile + ':' + sessionStorage.profile"
        )
        .unwrap(),
        JSValue::String("1:1".into())
    );
    server.close();
}
