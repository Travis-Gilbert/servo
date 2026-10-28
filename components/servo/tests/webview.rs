/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! WebView API unit tests.
mod common;

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, Mutex};

use dpi::PhysicalSize;
use embedder_traits::{RefreshDriver, UrlRequest};
use euclid::default::Size2D as UntypedSize2D;
use euclid::{Point2D, Scale, Size2D};
use http::{HeaderMap, HeaderName, HeaderValue};
use http_body_util::combinators::BoxBody;
use hyper::body::{Bytes, Incoming};
use hyper::{Request as HyperRequest, Response as HyperResponse};
use image::RgbaImage;
use itertools::Itertools;
use net::test_util::{make_body, make_server, replace_host_table};
use servo::profile_traits::mem::MemoryReportResult;
use servo::{
    CreateNewWebViewRequest, Cursor, EmbedderControl, InputEvent, InputMethodType, JSValue,
    LoadStatus, MouseButton, MouseLeftViewportEvent, MouseMoveEvent, PrefValue, RenderingContext,
    Scroll, SimpleDialog, Theme, WebView, WebViewBuilder, WebViewDelegate, WebViewPoint,
    WebViewVector,
};
use servo_base::generic_channel::GenericCallback;
use servo_config::prefs::Preferences;
use servo_url::ServoUrl;
use surfman::{Error, Surface, SurfaceTexture};
use url::Url;
use webrender_api::units::{DeviceIntRect, DeviceIntSize, DevicePoint, DeviceVector2D};

use crate::common::{
    ServoTest, WebViewDelegateImpl, click_at_point, evaluate_javascript,
    show_webview_and_wait_for_rendering_to_be_ready, wait_for_webview_scene_to_be_up_to_date,
};

/// The size of the display lists WebRender is holding.
fn retained_display_list_bytes(servo_test: &ServoTest) -> usize {
    let report: Arc<Mutex<Option<MemoryReportResult>>> = Arc::default();
    let report_from_callback = report.clone();
    let callback = GenericCallback::new(move |result| {
        if let Ok(result) = result {
            *report_from_callback.lock().unwrap() = Some(result);
        }
    })
    .expect("Should be able to create a memory report callback");
    servo_test.servo().create_memory_report(callback);

    let waiting = report.clone();
    servo_test.spin(move || waiting.lock().unwrap().is_none());

    let result = report
        .lock()
        .unwrap()
        .take()
        .expect("Should have a memory report once the spin loop ends");
    result
        .results
        .iter()
        .flat_map(|process| process.reports.iter())
        .filter(|report| report.path == ["webrender", "display-list"])
        .map(|report| report.size)
        .sum()
}

#[test]
fn test_create_webview() {
    let servo_test = ServoTest::new();
    let delegate = Rc::new(WebViewDelegateImpl::default());
    let webview = WebViewBuilder::new(servo_test.servo(), servo_test.rendering_context.clone())
        .delegate(delegate.clone())
        .build();

    servo_test.spin(move || !delegate.url_changed.get());

    let url = webview.url();
    assert!(url.is_some());
    assert_eq!(url.unwrap().to_string(), "about:blank");
}

/// A [`RenderingContext`] that provides no surfman connection, which the trait
/// permits: `connection()` returns `None`. Every other method must forward to the
/// inner context; a defaulted `prepare_for_rendering` would bind no framebuffer.
struct ConnectionlessRenderingContext(Rc<dyn RenderingContext>);

impl RenderingContext for ConnectionlessRenderingContext {
    fn prepare_for_rendering(&self) {
        self.0.prepare_for_rendering();
    }

    fn read_to_image(&self, source_rectangle: DeviceIntRect) -> Option<RgbaImage> {
        self.0.read_to_image(source_rectangle)
    }

    fn size(&self) -> PhysicalSize<u32> {
        self.0.size()
    }

    fn resize(&self, size: PhysicalSize<u32>) {
        self.0.resize(size);
    }

    fn present(&self) {
        self.0.present();
    }

    fn make_current(&self) -> Result<(), Error> {
        self.0.make_current()
    }

    fn gleam_gl_api(&self) -> Rc<dyn gleam::gl::Gl> {
        self.0.gleam_gl_api()
    }

    fn glow_gl_api(&self) -> Arc<glow::Context> {
        self.0.glow_gl_api()
    }

    fn create_texture(
        &self,
        surface: Surface,
    ) -> Option<(SurfaceTexture, u32, UntypedSize2D<i32>)> {
        self.0.create_texture(surface)
    }

    fn destroy_texture(&self, surface_texture: SurfaceTexture) -> Option<Surface> {
        self.0.destroy_texture(surface_texture)
    }

    fn refresh_driver(&self) -> Option<Rc<dyn RefreshDriver>> {
        self.0.refresh_driver()
    }
}

#[test]
fn test_create_webview_without_surfman_connection() {
    let servo_test = ServoTest::new();
    let rendering_context = Rc::new(ConnectionlessRenderingContext(
        servo_test.rendering_context.clone(),
    ));
    let delegate = Rc::new(WebViewDelegateImpl::default());
    let webview = WebViewBuilder::new(servo_test.servo(), rendering_context)
        .delegate(delegate.clone())
        .build();

    servo_test.spin(move || !delegate.url_changed.get());

    let url = webview.url();
    assert!(url.is_some());
    assert_eq!(url.unwrap().to_string(), "about:blank");
}

#[test]
fn test_create_webview_http() {
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

    let url = webview.url();
    assert!(url.is_some());
    let url = url.unwrap();
    assert_eq!(url.scheme(), "http");
    let host = url.host_str();
    assert!(host.is_some());
    assert_eq!(host.unwrap(), "localhost");
}

#[test]
fn test_create_webview_http_custom_host() {
    let servo_test = ServoTest::new();

    static MESSAGE: &'static [u8] = b"<!DOCTYPE html>\n<title>Hello</title>";
    let handler =
        move |_: HyperRequest<Incoming>,
              response: &mut HyperResponse<BoxBody<Bytes, hyper::Error>>| {
            *response.body_mut() = make_body(MESSAGE.to_vec());
        };
    let (server, url) = make_server(handler);
    let port = url.port().unwrap();

    let ip = "127.0.0.1".parse().unwrap();
    let mut host_table = HashMap::new();
    host_table.insert("www.example.com".to_owned(), ip);

    replace_host_table(host_table);

    let custom_url = ServoUrl::parse(&format!("http://www.example.com:{}", port)).unwrap();

    let delegate = Rc::new(WebViewDelegateImpl::default());

    let webview = WebViewBuilder::new(servo_test.servo(), servo_test.rendering_context.clone())
        .delegate(delegate.clone())
        .url(custom_url.clone().into_url())
        .build();

    servo_test.spin(move || !delegate.load_status_changed.get() || !delegate.url_changed.get());

    let _ = server.close();

    let page_title = webview.page_title();
    assert!(page_title.is_some());
    assert_eq!(page_title.unwrap(), "Hello");

    let url = webview.url();
    assert!(url.is_some());
    assert_eq!(url.unwrap(), custom_url.into_url());
}

#[test]
fn test_create_webview_and_immediately_drop_webview_before_shutdown() {
    let servo_test = ServoTest::new();
    WebViewBuilder::new(servo_test.servo(), servo_test.rendering_context.clone()).build();
}

#[test]
#[cfg_attr(
    target_os = "windows",
    ignore = "the scene is only observable through a memory report, which aborts \
              the process on Windows (#37238)"
)]
fn test_closing_a_webview_removes_its_pipelines_from_the_scene() {
    let servo_test = ServoTest::new();
    let page_url = Url::parse(
        "data:text/html,<!DOCTYPE html>\
            <style>div{width:100px;height:20px;background:red;margin:2px}</style>\
            <div></div><div></div><div></div><div></div><div></div>",
    )
    .unwrap();

    // Both `WebView`s share the rendering context:
    // `Paint::remove_webview` drops the painter after last `WebView` is gone,
    // so a closed `WebView`'s state is only observable while another is alive.
    let kept_delegate = Rc::new(WebViewDelegateImpl::default());
    let kept = WebViewBuilder::new(servo_test.servo(), servo_test.rendering_context.clone())
        .delegate(kept_delegate.clone())
        .url(page_url.clone())
        .build();
    show_webview_and_wait_for_rendering_to_be_ready(&servo_test, &kept, &kept_delegate);
    wait_for_webview_scene_to_be_up_to_date(&servo_test, &kept);

    let baseline = retained_display_list_bytes(&servo_test);

    let closed_delegate = Rc::new(WebViewDelegateImpl::default());
    let closed = WebViewBuilder::new(servo_test.servo(), servo_test.rendering_context.clone())
        .delegate(closed_delegate.clone())
        .url(page_url)
        .build();
    show_webview_and_wait_for_rendering_to_be_ready(&servo_test, &closed, &closed_delegate);
    wait_for_webview_scene_to_be_up_to_date(&servo_test, &closed);

    let with_both = retained_display_list_bytes(&servo_test);
    assert!(
        with_both > baseline,
        "A second loaded WebView should add retained display lists \
         ({baseline} bytes to {with_both} bytes)"
    );

    drop(closed);
    wait_for_webview_scene_to_be_up_to_date(&servo_test, &kept);

    // Compare against the baseline rather than `with_both`: the root display
    // list is rebuilt on removal, so a plain decrease would pass even if the
    // closed WebView's pipelines were never removed.
    let after = retained_display_list_bytes(&servo_test);
    assert!(
        after.abs_diff(baseline) < with_both.abs_diff(baseline) / 2,
        "Closing a WebView should return the scene to its single-WebView size \
         (baseline {baseline}, with both {with_both}, after closing {after} bytes)"
    );
}

#[test]
fn test_theme_change() {
    let servo_test = ServoTest::new();
    let delegate = Rc::new(WebViewDelegateImpl::default());
    let webview = WebViewBuilder::new(servo_test.servo(), servo_test.rendering_context.clone())
        .delegate(delegate.clone())
        .url(Url::parse("data:text/html,page one").unwrap())
        .build();

    let is_dark_theme_script = "window.matchMedia('(prefers-color-scheme: dark)').matches";

    // The default theme is "light".
    let result = evaluate_javascript(&servo_test, webview.clone(), is_dark_theme_script);
    assert_eq!(result, Ok(JSValue::Boolean(false)));

    // Changing the theme updates the current page.
    webview.notify_theme_change(Theme::Dark);
    let result = evaluate_javascript(&servo_test, webview.clone(), is_dark_theme_script);
    assert_eq!(result, Ok(JSValue::Boolean(true)));

    delegate.reset();
    webview.load(Url::parse("data:text/html,page two").unwrap());
    servo_test.spin(move || !delegate.url_changed.get());

    // The theme persists after a navigation.
    let result = evaluate_javascript(&servo_test, webview.clone(), is_dark_theme_script);
    assert_eq!(result, Ok(JSValue::Boolean(true)));

    // Now test the same thing, but setting the theme immediately after creating the WebView.
    let delegate = Rc::new(WebViewDelegateImpl::default());
    let webview = WebViewBuilder::new(servo_test.servo(), servo_test.rendering_context.clone())
        .delegate(delegate.clone())
        .url(Url::parse("data:text/html,page one").unwrap())
        .build();
    webview.notify_theme_change(Theme::Dark);
    let result = evaluate_javascript(&servo_test, webview.clone(), is_dark_theme_script);
    assert_eq!(result, Ok(JSValue::Boolean(true)));
}

#[test]
fn test_theme_change_media_query_event() {
    let servo_test = ServoTest::new();
    let delegate = Rc::new(WebViewDelegateImpl::default());
    let webview = WebViewBuilder::new(servo_test.servo(), servo_test.rendering_context.clone())
        .delegate(delegate.clone())
        .url(
            Url::parse(
                r#"data:text/html,<!DOCTYPE html><html><script>
            let mediaQueryChanges = 0;
            const mediaQueryList = window.matchMedia('(prefers-color-scheme: dark)');
            mediaQueryList.addEventListener('change', () => {
                mediaQueryChanges += 1;
                console.log(mediaQueryChanges);
            });
            </script></html>"#,
            )
            .unwrap(),
        )
        .build();

    // Check that we increment mediaQueryChanges each time the theme changes.

    show_webview_and_wait_for_rendering_to_be_ready(&servo_test, &webview, &delegate);
    let result = evaluate_javascript(&servo_test, webview.clone(), "mediaQueryChanges");
    assert_eq!(result, Ok(JSValue::Number(0.0)));

    webview.notify_theme_change(Theme::Dark);

    show_webview_and_wait_for_rendering_to_be_ready(&servo_test, &webview, &delegate);
    let result = evaluate_javascript(&servo_test, webview.clone(), "mediaQueryChanges");
    assert_eq!(result, Ok(JSValue::Number(1.0)));

    webview.notify_theme_change(Theme::Light);

    show_webview_and_wait_for_rendering_to_be_ready(&servo_test, &webview, &delegate);
    let result = evaluate_javascript(&servo_test, webview.clone(), "mediaQueryChanges");
    assert_eq!(result, Ok(JSValue::Number(2.0)));
}

#[test]
fn test_cursor_change() {
    let servo_test = ServoTest::new();
    let delegate = Rc::new(WebViewDelegateImpl::default());
    let webview = WebViewBuilder::new(servo_test.servo(), servo_test.rendering_context.clone())
        .delegate(delegate.clone())
        .url(
            Url::parse(
                "data:text/html,<!DOCTYPE html><style> html { cursor: crosshair; margin: 0}</style><body>hello</body>",
            )
            .unwrap(),
        )
        .build();

    show_webview_and_wait_for_rendering_to_be_ready(&servo_test, &webview, &delegate);

    webview.notify_input_event(InputEvent::MouseMove(MouseMoveEvent::new(
        DevicePoint::new(10., 10.).into(),
    )));

    let captured_delegate = delegate.clone();
    servo_test.spin(move || !captured_delegate.cursor_changed.get());
    assert_eq!(webview.cursor(), Cursor::Crosshair);

    delegate.reset();
    webview.notify_input_event(InputEvent::MouseLeftViewport(
        MouseLeftViewportEvent::default(),
    ));

    let captured_delegate = delegate.clone();
    servo_test.spin(move || !captured_delegate.cursor_changed.get());
    assert_eq!(webview.cursor(), Cursor::Default);
}

// A test to ensure that the cursor doesn't change when hovering over a input with type color
#[test]
fn test_cursor_unchanged_input_color() {
    let servo_test = ServoTest::new();
    let delegate = Rc::new(WebViewDelegateImpl::default());
    let webview = WebViewBuilder::new(servo_test.servo(), servo_test.rendering_context.clone())
        .delegate(delegate.clone())
        .url(
            Url::parse(
                "data:text/html,<!DOCTYPE html><body><input type=\"color\"><p>Test text for Cursor change</p></body>",
            )
            .unwrap(),
        )
        .build();

    show_webview_and_wait_for_rendering_to_be_ready(&servo_test, &webview, &delegate);

    webview.notify_input_event(InputEvent::MouseMove(MouseMoveEvent::new(
        DevicePoint::new(20., 65.).into(),
    )));

    let captured_delegate = delegate.clone();
    servo_test.spin(move || !captured_delegate.cursor_changed.get());
    assert_eq!(webview.cursor(), Cursor::Text);

    delegate.reset();
    webview.notify_input_event(InputEvent::MouseMove(MouseMoveEvent::new(
        DevicePoint::new(20., 25.).into(),
    )));

    let captured_delegate = delegate.clone();
    servo_test.spin(move || !captured_delegate.cursor_changed.get());
    assert_eq!(webview.cursor(), Cursor::Default);
}

/// A test that ensure that negative resize requests do not get passed to the embedder.
#[test]
fn test_negative_resize_to_request() {
    let servo_test = ServoTest::new();
    struct WebViewResizeTestDelegate {
        rendering_context: Rc<dyn RenderingContext>,
        popup: RefCell<Option<WebView>>,
        resize_request: Cell<Option<DeviceIntSize>>,
    }

    impl WebViewDelegate for WebViewResizeTestDelegate {
        fn request_create_new(&self, parent_webview: WebView, request: CreateNewWebViewRequest) {
            let webview = request
                .builder(self.rendering_context.clone())
                .delegate(parent_webview.delegate())
                .build();
            self.popup.borrow_mut().replace(webview.clone());
        }

        fn request_resize_to(&self, _: WebView, requested_outer_size: DeviceIntSize) {
            self.resize_request.set(Some(requested_outer_size));
        }
    }

    let delegate = Rc::new(WebViewResizeTestDelegate {
        rendering_context: servo_test.rendering_context.clone(),
        popup: None.into(),
        resize_request: None.into(),
    });

    let webview = WebViewBuilder::new(servo_test.servo(), servo_test.rendering_context.clone())
        .delegate(delegate.clone())
        .url(
            Url::parse(
                "data:text/html,<!DOCTYPE html><script>\
                    let popup = window.open('about:blank');\
                    popup.resizeTo(-100, -100);\
                </script></body>",
            )
            .unwrap(),
        )
        .build();

    let load_webview = webview.clone();
    let _ = servo_test.spin(move || load_webview.load_status() != LoadStatus::Complete);

    let popup = delegate
        .popup
        .borrow()
        .clone()
        .expect("Should have created popup");

    let load_webview = popup.clone();
    let _ = servo_test.spin(move || load_webview.load_status() != LoadStatus::Complete);

    // Resize requests should be floored to 1.
    assert_eq!(
        delegate.resize_request.get(),
        Some(DeviceIntSize::new(1, 1))
    );

    // Ensure that the popup WebView is released before the end of the test.
    *delegate.popup.borrow_mut() = None;
}

/// This test verifies that trying to set the WebView size to a negative value does
/// not crash Servo.
#[test]
fn test_resize_webview_zero() {
    let servo_test = ServoTest::new();
    let delegate = Rc::new(WebViewDelegateImpl::default());
    let webview = WebViewBuilder::new(servo_test.servo(), servo_test.rendering_context.clone())
        .delegate(delegate.clone())
        .url(Url::parse("data:text/html,<!DOCTYPE html><body>hello</body>").unwrap())
        .build();
    webview.resize(PhysicalSize::new(0, 0));

    let load_webview = webview.clone();
    let _ = servo_test.spin(move || load_webview.load_status() != LoadStatus::Complete);

    // Reset the WebView size for other tests.
    webview.resize(PhysicalSize::new(500, 500));
}

/// This test ensure's that when a `WebView` is resize, input event are handled properly in the newly
/// exposed region.
#[test]
fn test_webview_resize_interactivity() {
    let servo_test = ServoTest::new();
    let delegate = Rc::new(WebViewDelegateImpl::default());
    let webview = WebViewBuilder::new(servo_test.servo(), servo_test.rendering_context.clone())
        .delegate(delegate.clone())
        .url(
            Url::parse(
                "data:text/html,<!DOCTYPE html>\
                <style>\
                    html { margin: 0; }\
                    div { margin-top: 500px; width: 100px; height: 100px; cursor: crosshair; }\
                </style>\
                <body><div></div></body>",
            )
            .unwrap(),
        )
        .build();

    show_webview_and_wait_for_rendering_to_be_ready(&servo_test, &webview, &delegate);

    // Resize the WebView to expose the `<div>`.
    assert_eq!(webview.size(), Size2D::new(500., 500.));
    webview.resize(PhysicalSize::new(600, 600));

    wait_for_webview_scene_to_be_up_to_date(&servo_test, &webview);

    webview.notify_input_event(InputEvent::MouseMove(MouseMoveEvent::new(
        DevicePoint::new(20., 520.).into(),
    )));

    let captured_delegate = delegate.clone();
    servo_test.spin(move || !captured_delegate.cursor_changed.get());
    assert_eq!(webview.cursor(), Cursor::Crosshair);
}

#[test]
fn test_control_show_and_hide() {
    let servo_test = ServoTest::new();
    let delegate = Rc::new(WebViewDelegateImpl::default());
    let webview = WebViewBuilder::new(servo_test.servo(), servo_test.rendering_context.clone())
        .delegate(delegate.clone())
        .url(
            Url::parse(
                "data:text/html,\
                    <!DOCTYPE html>\
                        <select id=select style=\"width: 500px; height: 500px\">\
                            <option>one</option>\
                        </select>
                        <script>\
                            select.addEventListener('click', () => {\
                                setTimeout(() => select.parentNode.removeChild(select), 100);\
                            });\
                        </script>",
            )
            .unwrap(),
        )
        .build();

    show_webview_and_wait_for_rendering_to_be_ready(&servo_test, &webview, &delegate);
    click_at_point(&webview, Point2D::new(50., 50.), MouseButton::Primary);

    // The form control should be shown and then immediately hidden.
    let captured_delegate = delegate.clone();
    servo_test.spin(move || captured_delegate.number_of_controls_shown.get() != 1);
    let captured_delegate = delegate.clone();
    servo_test.spin(move || captured_delegate.number_of_controls_hidden.get() != 1);
}

#[test]
fn test_page_zoom() {
    let servo_test = ServoTest::new();
    let delegate = Rc::new(WebViewDelegateImpl::default());
    let webview = WebViewBuilder::new(servo_test.servo(), servo_test.rendering_context.clone())
        .delegate(delegate.clone())
        .build();

    // Default zoom should be 1.0
    assert_eq!(webview.page_zoom(), 1.0);

    webview.set_page_zoom(1.5);
    assert_eq!(webview.page_zoom(), 1.5);

    webview.set_page_zoom(0.5);
    assert_eq!(webview.page_zoom(), 0.5);

    // Should clamp to minimum
    webview.set_page_zoom(-1.0);
    assert_eq!(webview.page_zoom(), 0.1);

    // Should clamp to maximum
    webview.set_page_zoom(100.0);
    assert_eq!(webview.page_zoom(), 10.0);
}

fn observe_viewport(servo_test: &ServoTest, webview: &WebView) {
    assert_eq!(
        evaluate_javascript(
            servo_test,
            webview.clone(),
            r#"
            window.viewportSamples = [];
            window.resizeSamples = [];
            window.visualResizeSamples = [];
            window.resolutionChanges = [];
            window.resolutionQuery = matchMedia('(resolution: 1.25dppx)');
            const sample = () => [innerWidth, innerHeight, devicePixelRatio];
            addEventListener('resize', () => resizeSamples.push(sample()));
            if (window.visualViewport) {
                visualViewport.addEventListener('resize', () => visualResizeSamples.push(
                    [visualViewport.width, visualViewport.height]));
            }
            resolutionQuery.addEventListener('change', () => resolutionChanges.push(sample()));
            function frame() {
                viewportSamples.push(sample());
                requestAnimationFrame(frame);
            }
            requestAnimationFrame(frame);
            true;
        "#
        ),
        Ok(JSValue::Boolean(true)),
    );
}

#[test]
fn test_set_viewport_scale_only() {
    let servo_test = ServoTest::new_with_builder(|builder| {
        let mut preferences = Preferences::default();
        preferences.dom_visual_viewport_enabled = true;
        builder.preferences(preferences)
    });
    let delegate = Rc::new(WebViewDelegateImpl::default());
    let webview = WebViewBuilder::new(servo_test.servo(), servo_test.rendering_context.clone())
        .delegate(delegate.clone())
        .build();
    webview.set_viewport(Size2D::new(641.0, 403.0), Scale::new(1.0));
    show_webview_and_wait_for_rendering_to_be_ready(&servo_test, &webview, &delegate);
    wait_for_webview_scene_to_be_up_to_date(&servo_test, &webview);
    observe_viewport(&servo_test, &webview);

    // The first two scales produce the same rounded device surface.
    for scale in [1.25, 1.2501, 0.7, 2.0, 1.0] {
        delegate.new_frame_ready.set(false);
        webview.set_viewport(Size2D::new(641.0, 403.0), Scale::new(scale));
        assert_eq!(
            webview.size(),
            Size2D::new((641.0 * scale).ceil(), (403.0 * scale).ceil()),
        );
        wait_for_webview_scene_to_be_up_to_date(&servo_test, &webview);
        servo_test.spin(|| !delegate.new_frame_ready.get());
        assert_eq!(
            evaluate_javascript(
                &servo_test,
                webview.clone(),
                format!(
                    r#"
                innerWidth === 641 && innerHeight === 403 &&
                Math.abs(devicePixelRatio - {scale}) < 0.000001 &&
                resizeSamples.length === 0 &&
                visualResizeSamples.length === 0 &&
                visualViewport.width === 641 && visualViewport.height === 403 &&
                viewportSamples.length > 0 &&
                viewportSamples.every(s => s[0] === 641 && s[1] === 403) &&
                resolutionQuery.matches === ({scale} === 1.25)
            "#
                )
            ),
            Ok(JSValue::Boolean(true)),
        );
    }
    assert_eq!(
        evaluate_javascript(
            &servo_test,
            webview.clone(),
            r#"
            resolutionChanges.length === 2 &&
            resolutionChanges.every(s => s[0] === 641 && s[1] === 403)
        "#
        ),
        Ok(JSValue::Boolean(true)),
    );
}

#[test]
fn test_set_viewport_css_resize() {
    let servo_test = ServoTest::new();
    let delegate = Rc::new(WebViewDelegateImpl::default());
    let webview = WebViewBuilder::new(servo_test.servo(), servo_test.rendering_context.clone())
        .delegate(delegate.clone())
        .build();
    show_webview_and_wait_for_rendering_to_be_ready(&servo_test, &webview, &delegate);
    wait_for_webview_scene_to_be_up_to_date(&servo_test, &webview);
    observe_viewport(&servo_test, &webview);

    webview.set_viewport(Size2D::new(641.0, 403.0), Scale::new(1.25));
    wait_for_webview_scene_to_be_up_to_date(&servo_test, &webview);
    assert_eq!(
        evaluate_javascript(
            &servo_test,
            webview.clone(),
            r#"
            innerWidth === 641 && innerHeight === 403 && devicePixelRatio === 1.25 &&
            JSON.stringify(resizeSamples) === '[[641,403,1.25]]' &&
            viewportSamples.every(s =>
                (s[0] === 500 && s[1] === 500 && s[2] === 1) ||
                (s[0] === 641 && s[1] === 403 && s[2] === 1.25))
        "#
        ),
        Ok(JSValue::Boolean(true)),
    );

    // Repeating the same update must not create another resize event.
    webview.set_viewport(Size2D::new(641.0, 403.0), Scale::new(1.25));
    wait_for_webview_scene_to_be_up_to_date(&servo_test, &webview);
    assert_eq!(
        evaluate_javascript(&servo_test, webview.clone(), "resizeSamples.length"),
        Ok(JSValue::Number(1.0)),
    );
}

#[test]
fn test_set_viewport_then_legacy_resize_and_scale() {
    let servo_test = ServoTest::new();
    let delegate = Rc::new(WebViewDelegateImpl::default());
    let webview = WebViewBuilder::new(servo_test.servo(), servo_test.rendering_context.clone())
        .delegate(delegate.clone())
        .build();
    webview.set_viewport(Size2D::new(641.0, 403.0), Scale::new(1.25));
    show_webview_and_wait_for_rendering_to_be_ready(&servo_test, &webview, &delegate);

    webview.resize(PhysicalSize::new(1000, 750));
    wait_for_webview_scene_to_be_up_to_date(&servo_test, &webview);
    assert_eq!(
        evaluate_javascript(
            &servo_test,
            webview.clone(),
            "[innerWidth, innerHeight, devicePixelRatio]"
        ),
        Ok(JSValue::Array(vec![
            JSValue::Number(800.0),
            JSValue::Number(600.0),
            JSValue::Number(1.25)
        ])),
    );

    webview.set_viewport(Size2D::new(641.0, 403.0), Scale::new(1.25));
    webview.set_hidpi_scale_factor(Scale::new(1.0));
    wait_for_webview_scene_to_be_up_to_date(&servo_test, &webview);
    assert_eq!(
        evaluate_javascript(
            &servo_test,
            webview.clone(),
            "[innerWidth, innerHeight, devicePixelRatio]"
        ),
        Ok(JSValue::Array(vec![
            JSValue::Number(802.0),
            JSValue::Number(504.0),
            JSValue::Number(1.0)
        ])),
    );
}

#[test]
fn test_viewport_meta_tag_initial_scale() {
    let servo_test = ServoTest::new_with_builder(|builder| {
        let mut preferences = Preferences::default();
        preferences.viewport_meta_enabled = true;
        preferences.dom_visual_viewport_enabled = true;
        builder.preferences(preferences)
    });

    let initial_scale: f64 = 2.0;

    let delegate = Rc::new(WebViewDelegateImpl::default());
    let webview = WebViewBuilder::new(servo_test.servo(), servo_test.rendering_context.clone())
        .delegate(delegate.clone())
        .url(
            Url::parse(
                format!(
                    "data:text/html,\
                    <!DOCTYPE html>\
                    <meta name=viewport content=\"initial-scale={}\">",
                    initial_scale
                )
                .as_str(),
            )
            .unwrap(),
        )
        .build();

    // Initially HiDPI should be 1.0
    assert_eq!(webview.hidpi_scale_factor().get(), 1.0);

    show_webview_and_wait_for_rendering_to_be_ready(&servo_test, &webview, &delegate);

    // HiDPI should be same after parsing the viewport meta from page.
    assert_eq!(webview.hidpi_scale_factor().get(), 1.0);

    // devicePixelRatio should be unaffected by Meta Tag.
    assert_eq!(
        Ok(JSValue::Number(webview.hidpi_scale_factor().get() as f64)),
        evaluate_javascript(&servo_test, webview.clone(), "window.devicePixelRatio;")
    );

    // Meta Tag should affect VisualViewport Scale.
    assert_eq!(
        Ok(JSValue::Number(initial_scale)),
        evaluate_javascript(&servo_test, webview.clone(), "window.visualViewport.scale;")
    );
}

#[test]
fn test_show_and_hide_ime() {
    let servo_test = ServoTest::new();

    let delegate = Rc::new(WebViewDelegateImpl::default());
    let webview = WebViewBuilder::new(servo_test.servo(), servo_test.rendering_context.clone())
        .delegate(delegate.clone())
        .url(
            Url::parse(
                "data:text/html,<!DOCTYPE html> \
                <input type=\"text\" value=\"servo\" style=\"width: 200px; height: 200px;\">",
            )
            .unwrap(),
        )
        .build();

    show_webview_and_wait_for_rendering_to_be_ready(&servo_test, &webview, &delegate);
    click_at_point(&webview, Point2D::new(150., 150.), MouseButton::Primary);

    // The form control should be shown.
    let captured_delegate = delegate.clone();
    servo_test.spin(move || captured_delegate.number_of_controls_shown.get() != 1);

    {
        let controls = delegate.controls_shown.borrow();
        assert_eq!(controls.len(), 1);
        let EmbedderControl::InputMethod(ime) = &controls[0] else {
            unreachable!("Expected embedder control to be an IME");
        };

        assert_eq!(ime.input_method_type(), InputMethodType::Text);
        assert_eq!(ime.text(), "servo");
        assert_eq!(ime.insertion_point(), Some(5));
    }

    click_at_point(&webview, Point2D::new(300., 300.), MouseButton::Primary);

    // The form control should be hidden when the field no longer has focus.
    let captured_delegate = delegate.clone();
    servo_test.spin(move || captured_delegate.number_of_controls_hidden.get() != 1);
}

#[test]
fn test_alert_dialog() {
    test_simple_dialog("window.alert('Alert');", |dialog| {
        let SimpleDialog::Alert(..) = dialog else {
            unreachable!("Expected dialog to be a SimpleDialog::Alert");
        };
        assert_eq!(dialog.message(), "Alert");
    });
}

#[test]
fn test_prompt_dialog() {
    test_simple_dialog("window.prompt('Prompt');", |dialog| {
        let SimpleDialog::Prompt(..) = dialog else {
            unreachable!("Expected dialog to be a SimpleDialog::Prompt");
        };
        assert_eq!(dialog.message(), "Prompt");
    });
}

#[test]
fn test_confirm_dialog() {
    test_simple_dialog("window.confirm('Confirm');", |dialog| {
        let SimpleDialog::Confirm(..) = dialog else {
            unreachable!("Expected dialog to be a SimpleDialog::Confirm");
        };
        assert_eq!(dialog.message(), "Confirm");
    });
}

// A helper function to share code among the dialog tests.
//
// `prompt` must be a string that can be used in the `onclick` attribute and therefore must
// be escaped correctly. Use single quotes instead of double quotes for string literals.
fn test_simple_dialog(prompt: &str, validate: impl Fn(&SimpleDialog)) {
    let make_test_html = |prompt: &str| {
        let html = format!(
            "data:text/html,<!DOCTYPE html>\
            <input type=\"button\" value=\"click\" style=\"width: 200px; height: 200px;\"\
            onclick=\"{}\">",
            prompt
        );
        Url::parse(&html).unwrap()
    };

    let servo_test = ServoTest::new();
    let delegate = Rc::new(WebViewDelegateImpl::default());
    let webview = WebViewBuilder::new(servo_test.servo(), servo_test.rendering_context.clone())
        .delegate(delegate.clone())
        .url(make_test_html(prompt))
        .build();

    show_webview_and_wait_for_rendering_to_be_ready(&servo_test, &webview, &delegate);

    // The dialog should NOT be shown.
    assert!(delegate.active_dialog.borrow().is_none());

    click_at_point(&webview, Point2D::new(100., 100.), MouseButton::Primary);
    let captured_delegate = delegate.clone();
    servo_test.spin(move || captured_delegate.active_dialog.borrow().is_none());

    let active_dialog = delegate.active_dialog.borrow();
    validate(
        active_dialog
            .as_ref()
            .expect("the spin call above ensures this is not None"),
    );
}

#[test]
fn test_can_go_forward_and_can_go_back() {
    let servo_test = ServoTest::new();

    let page_1_url = Url::parse("data:text/html,<!DOCTYPE html> page 1").unwrap();
    let page_2_url = Url::parse("data:text/html,<!DOCTYPE html> page 2").unwrap();

    let delegate = Rc::new(WebViewDelegateImpl::default());
    let webview = WebViewBuilder::new(servo_test.servo(), servo_test.rendering_context.clone())
        .delegate(delegate.clone())
        .url(page_1_url.clone())
        .build();

    show_webview_and_wait_for_rendering_to_be_ready(&servo_test, &webview, &delegate);

    assert!(!webview.can_go_forward());
    assert!(!webview.can_go_back());

    let load_webview = webview.clone();
    webview.load(page_2_url.clone());
    servo_test.spin(move || load_webview.url() != Some(page_2_url.clone()));

    assert!(!webview.can_go_forward());
    assert!(webview.can_go_back());

    webview.go_back(1);

    let load_webview = webview.clone();
    servo_test.spin(move || load_webview.url() != Some(page_1_url.clone()));

    assert!(webview.can_go_forward());
    assert!(!webview.can_go_back());
}

#[test]
fn test_pinch_zoom_update_dom_visual_viewport() {
    let servo_test = ServoTest::new_with_builder(|builder| {
        let mut preferences = Preferences::default();
        preferences.dom_visual_viewport_enabled = true;
        builder.preferences(preferences)
    });

    let delegate = Rc::new(WebViewDelegateImpl::default());
    let webview = WebViewBuilder::new(servo_test.servo(), servo_test.rendering_context.clone())
        .delegate(delegate.clone())
        .url(Url::parse("data:text/html,<!DOCTYPE html><body>Hello world!</body>").unwrap())
        .build();

    show_webview_and_wait_for_rendering_to_be_ready(&servo_test, &webview, &delegate);
    let eval_visual_viewport = |attribute: &str| match evaluate_javascript(
        &servo_test,
        webview.clone(),
        format!("window.visualViewport.{}", attribute),
    ) {
        Ok(JSValue::Number(number)) => Some(number),
        _ => None,
    };

    // Default value of the DOM visual viewport is initialized correctly.
    assert_eq!(eval_visual_viewport("scale"), Some(1.));
    assert_eq!(eval_visual_viewport("width"), Some(500.));
    assert_eq!(eval_visual_viewport("height"), Some(500.));
    assert_eq!(eval_visual_viewport("offsetLeft"), Some(0.));
    assert_eq!(eval_visual_viewport("offsetTop"), Some(0.));

    webview.adjust_pinch_zoom(5., DevicePoint::new(100., 100.));
    wait_for_webview_scene_to_be_up_to_date(&servo_test, &webview);

    // The visual viewport size and offset is correct after a pinch zoom.
    assert_eq!(eval_visual_viewport("scale"), Some(5.));
    assert_eq!(eval_visual_viewport("width"), Some(100.));
    assert_eq!(eval_visual_viewport("height"), Some(100.));
    assert_eq!(eval_visual_viewport("offsetLeft"), Some(80.));
    assert_eq!(eval_visual_viewport("offsetTop"), Some(80.));

    // Note that the scroll vector will be affected by the pinch zoom scale.
    webview.notify_scroll_event(
        Scroll::Delta(WebViewVector::Device(DeviceVector2D::new(100., 100.))),
        WebViewPoint::Device(DevicePoint::zero()),
    );
    wait_for_webview_scene_to_be_up_to_date(&servo_test, &webview);

    // The visual viewport size and offset is correct after the scroll event.
    assert_eq!(eval_visual_viewport("scale"), Some(5.));
    assert_eq!(eval_visual_viewport("width"), Some(100.));
    assert_eq!(eval_visual_viewport("height"), Some(100.));
    assert_eq!(eval_visual_viewport("offsetLeft"), Some(100.));
    assert_eq!(eval_visual_viewport("offsetTop"), Some(100.));
}

#[test]
fn test_webview_load_with_headers() {
    let servo_test = ServoTest::new();
    let request_count = Arc::new(AtomicUsize::new(0));
    let request_count_clone = request_count.clone();

    let mut headers = HeaderMap::new();
    headers.append(HeaderName::from_static("world"), "hello".parse().unwrap());
    headers.append(HeaderName::from_static("second"), "header".parse().unwrap());
    headers.append(
        HeaderName::from_static("accept-encoding"),
        "customzip".parse().unwrap(),
    );
    static MESSAGE: &'static [u8] = b"<!DOCTYPE html>\nHello";
    let handler =
        move |req: HyperRequest<Incoming>,
              response: &mut HyperResponse<BoxBody<Bytes, hyper::Error>>| {
            if request_count_clone.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 2 {
                let headers = req.headers();
                assert_eq!(
                    headers.get("world"),
                    Some(&HeaderValue::from_static("hello"))
                );
                assert_eq!(
                    headers.get("second"),
                    Some(&HeaderValue::from_static("header"))
                );

                // headers should be added if they already exist.
                assert!(
                    headers
                        .get_all("accept-encoding")
                        .iter()
                        .contains(&HeaderValue::from_static("customzip"))
                );
                assert!(headers.get_all("accept-encoding").iter().count() > 1);
            } else {
                *response.body_mut() = make_body(MESSAGE.to_vec());
            }
        };
    let (server, url) = make_server(handler);

    let delegate = Rc::new(WebViewDelegateImpl::default());

    let webview = WebViewBuilder::new(servo_test.servo(), servo_test.rendering_context.clone())
        .delegate(delegate.clone())
        .url(url.url().into_url())
        .build();

    {
        let delegate = delegate.clone();
        servo_test.spin(move || !delegate.url_changed.get());
    }

    let urlrequest = UrlRequest::new(url.url().into_url()).headers(headers);
    webview.load_request(urlrequest);
    {
        let request_count = request_count.clone();
        servo_test.spin(move || request_count.load(std::sync::atomic::Ordering::SeqCst) < 2);
    }
    assert_eq!(request_count.load(std::sync::atomic::Ordering::SeqCst), 2);
    server.close();
}

#[test]
fn test_console_log_and_error_ordering() {
    let servo_test = ServoTest::new();
    let delegate = Rc::new(WebViewDelegateImpl::default());

    let _webview = WebViewBuilder::new(servo_test.servo(), servo_test.rendering_context.clone())
        .delegate(delegate.clone())
        .url(
            Url::parse(
                "data:text/html,<!doctype html><script>\
                    console.log('Hello');\
                    DocumentFragment.prototype.querySelector.call(document, 'body');\
                </script>",
            )
            .unwrap(),
        )
        .build();

    // Wait until both messages have arrived through the embedder channel.
    let captured = delegate.clone();
    servo_test.spin(move || captured.console_messages.borrow().len() < 2);

    let messages = delegate.console_messages.borrow();
    assert_eq!(
        messages.len(),
        2,
        "Expected exactly 2 console messages, got: {messages:?}"
    );

    // The console.log("Hello") must arrive first.
    assert!(
        messages[0].1.contains("Hello"),
        "Expected first message to contain 'Hello', got: {:?}",
        messages[0].1
    );

    // The uncaught TypeError must arrive second.
    assert!(
        messages[1].1.contains("DocumentFragment") || messages[1].1.contains("querySelector"),
        "Expected second message to contain error info, got: {:?}",
        messages[1].1
    );
}

#[test]
fn test_preferences_change() {
    let servo_test = ServoTest::new();
    // Exercise the disabled-to-enabled transition independently of the fork's default.
    servo_test
        .servo()
        .set_preference("layout_grid_enabled", PrefValue::Bool(false));
    let delegate = Rc::new(WebViewDelegateImpl::default());

    let test_page =
        Url::parse("data:text/html,<div id=\"target\" style=\"backdrop-filter: sepia(1)\"></div>")
            .expect("Data URL failed to build");

    let webview = WebViewBuilder::new(servo_test.servo(), servo_test.rendering_context.clone())
        .delegate(delegate.clone())
        .url(test_page.clone())
        .build();
    show_webview_and_wait_for_rendering_to_be_ready(&servo_test, &webview, &delegate);

    assert_eq!(
        // The backdrop-filter style is feature flagged by the layout.unimplemented feature,
        // so when layout.unimplemented feature is disabled, the backdrop-filter style specified
        // in the stylesheet won't parse and the computed value undefined
        Ok(JSValue::Array(vec![
            JSValue::String(String::new()),
            JSValue::String(String::new())
        ])),
        evaluate_javascript(
            &servo_test,
            webview.clone(),
            "let old_value = target.style.getPropertyValue('backdrop-filter');
            target.style.backdropFilter = 'sepia(1)';
            let new_value = target.style.getPropertyValue('backdrop-filter');
            [old_value, new_value]"
        )
    );

    servo_test
        .servo()
        .set_preference("layout_unimplemented", PrefValue::Bool(true));

    webview.reload();

    assert_eq!(
        // When layout.unimplemented feature is enabled, the backdrop-filter style specified in
        // the stylesheet will parse and the computed value will be that value
        Ok(JSValue::Array(vec![
            JSValue::String("sepia(1)".to_string()),
            JSValue::String("opacity(0.5)".to_string())
        ])),
        evaluate_javascript(
            &servo_test,
            webview,
            "let old_value = target.style.getPropertyValue('backdrop-filter');
            target.style.backdropFilter = 'opacity(0.5)';
            let new_value = target.style.getPropertyValue('backdrop-filter');
            [old_value, new_value]"
        )
    );
}

#[test]
fn test_fullscreen() {
    let servo_test = ServoTest::new();
    let delegate = Rc::new(WebViewDelegateImpl::default());

    let test_page = Url::parse(
        "data:text/html,\
        <button onclick='this.requestFullscreen()'>click</button>\
        <a href=about:blank>click</a>",
    )
    .expect("Data URL failed to build");

    let webview = WebViewBuilder::new(servo_test.servo(), servo_test.rendering_context.clone())
        .delegate(delegate.clone())
        .url(test_page.clone())
        .build();
    show_webview_and_wait_for_rendering_to_be_ready(&servo_test, &webview, &delegate);

    click_at_point(&webview, Point2D::new(10., 10.), MouseButton::Primary);

    let captured = delegate.clone();
    servo_test.spin(move || !captured.fullscreen.get());

    assert!(
        evaluate_javascript(
            &servo_test,
            webview.clone(),
            "document.querySelector('a').click();"
        )
        .is_ok()
    );

    let captured = delegate.clone();
    servo_test.spin(move || captured.fullscreen.get());
}

#[test]
fn test_webview_title_updates_when_document_title_is_updated() {
    let servo_test = ServoTest::new();
    let delegate = Rc::new(WebViewDelegateImpl::default());
    let test_page = Url::parse(
        "data:text/html,\
        <script>document.title='Success';</script>",
    )
    .expect("Data URL failed to build");

    let webview = WebViewBuilder::new(servo_test.servo(), servo_test.rendering_context.clone())
        .delegate(delegate.clone())
        .url(test_page.clone())
        .build();

    // Wait for the page to load
    let load_webview = webview.clone();
    servo_test.spin(move || load_webview.load_status() != LoadStatus::Complete);

    assert_eq!(webview.page_title().as_deref(), Some("Success"));
}

#[test]
fn test_webview_title_updates_when_title_element_is_created_from_javascript() {
    let servo_test = ServoTest::new();
    let delegate = Rc::new(WebViewDelegateImpl::default());
    let test_page = Url::parse(
        "data:text/html,\
        <body>\
        <script>\
        let title = document.createElement('title');\
        title.textContent = 'Success';\
        document.body.appendChild(title);\
        </script>",
    )
    .expect("Data URL failed to build");

    let webview = WebViewBuilder::new(servo_test.servo(), servo_test.rendering_context.clone())
        .delegate(delegate.clone())
        .url(test_page.clone())
        .build();

    // Wait for the page to load
    let load_webview = webview.clone();
    servo_test.spin(move || load_webview.load_status() != LoadStatus::Complete);

    assert_eq!(webview.page_title().as_deref(), Some("Success"));
}

#[test]
fn test_webview_clear_history() {
    let session_history_changed = Rc::new(Cell::new(false));
    let entries = Rc::new(RefCell::new(vec![]));
    struct MyDelegate {
        session_history_changed: Rc<Cell<bool>>,
        entries: Rc<RefCell<Vec<Url>>>,
    }
    impl WebViewDelegate for MyDelegate {
        fn notify_history_changed(&self, _webview: WebView, entries: Vec<Url>, _current: usize) {
            self.session_history_changed.set(true);
            *self.entries.borrow_mut() = entries;
        }
    }

    let servo_test = ServoTest::new();
    let delegate = Rc::new(MyDelegate {
        session_history_changed: session_history_changed.clone(),
        entries: entries.clone(),
    });

    let url_1 = Url::parse("data:text/html,<body><title>Success</title></body>").unwrap();
    let webview = WebViewBuilder::new(servo_test.servo(), servo_test.rendering_context.clone())
        .delegate(delegate.clone())
        .url(url_1)
        .build();
    {
        let webview = webview.clone();
        servo_test.spin(move || webview.page_title() != Some("Success".into()));
    }
    let second_url =
        Url::parse("data:text/html,<script>document.title='Success2';</script>").unwrap();
    webview.load(second_url.clone());
    {
        let webview = webview.clone();
        servo_test.spin(move || webview.page_title() != Some("Success2".into()));
    }

    webview.load(Url::parse("data:text/html,<script>document.title='Success3';</script>").unwrap());
    {
        let webview = webview.clone();
        servo_test.spin(move || webview.page_title() != Some("Success3".into()));
    }

    assert!(webview.can_go_back());

    webview.go_back(1);
    {
        let webview = webview.clone();
        servo_test.spin(move || webview.page_title() != Some("Success2".into()));
    }

    session_history_changed.set(false);
    entries.borrow_mut().clear();
    webview.clear_session_history();

    servo_test.spin(move || !session_history_changed.get());

    assert!(!webview.can_go_back());
    assert!(!webview.can_go_forward());
    assert_eq!(&*entries.borrow(), &vec![second_url]);
}

#[test]
fn test_hide_animating_webview() {
    let servo_test = ServoTest::new();
    let delegate = Rc::new(WebViewDelegateImpl::default());
    let webview = WebViewBuilder::new(servo_test.servo(), servo_test.rendering_context.clone())
        .delegate(delegate.clone())
        .url(Url::parse("data:text/html,<script>(function frame() { requestAnimationFrame(frame) })();</script>").unwrap())
        .build();

    // Show the WebView and wait for it to start animating.
    show_webview_and_wait_for_rendering_to_be_ready(&servo_test, &webview, &delegate);
    servo_test.spin(|| !webview.animating());

    webview.hide();
    servo_test.spin(|| webview.animating());
}

fn partition_async_result(test: &ServoTest, view: &WebView, body: &str) -> String {
    // Include page load and each evaluation callback in the deadline; the general
    // test helper waits indefinitely, hiding the stage of a stalled worker test.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(8);
    let evaluate = |script: String| {
        test.spin(|| {
            assert!(
                std::time::Instant::now() < deadline,
                "partition page load timed out: {body}"
            );
            view.load_status() != LoadStatus::Complete
        });
        let result = Rc::new(RefCell::new(None));
        let callback_result = result.clone();
        view.evaluate_javascript(script, move |value| {
            *callback_result.borrow_mut() = Some(value);
        });
        test.spin(|| {
            assert!(
                std::time::Instant::now() < deadline,
                "partition JavaScript evaluation timed out: {body}"
            );
            result.borrow().is_none()
        });
        let value = result.borrow_mut().take().unwrap().unwrap();
        value
    };
    evaluate(format!(
        "window.partitionResult = null; (async () => {{ {body} }})().then(value => window.partitionResult = String(value), error => window.partitionResult = 'ERROR: ' + error);"
    ));
    loop {
        if let JSValue::String(value) = evaluate("window.partitionResult".into()) {
            assert!(!value.starts_with("ERROR:"), "{value}");
            return value;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "partition operation timed out: {body}"
        );
        test.servo().spin_event_loop();
    }
}

#[test]
fn test_storage_partitions_share_only_within_profile_and_popups_inherit() {
    let test = ServoTest::new();
    let (server, url) = make_server(|request, response| {
        let body = if request.uri().path() == "/sandbox" {
            let cookie = request.headers()[http::header::COOKIE].to_str().unwrap();
            format!("<!doctype html><script>parent.postMessage('{cookie}', '*')</script>")
        } else if request.uri().path() == "/cookie" {
            request
                .headers()
                .get(http::header::COOKIE)
                .map_or("", |cookie| cookie.to_str().unwrap())
                .to_owned()
        } else {
            "<!doctype html><title>Partitions</title>".into()
        };
        *response.body_mut() = make_body(body.into_bytes());
    });
    let a = test.servo().create_ephemeral_storage_partition().unwrap();
    let b = test.servo().create_ephemeral_storage_partition().unwrap();
    let make_view = |partition| {
        WebViewBuilder::new(test.servo(), test.rendering_context.clone())
            .storage_partition(partition)
            .url(Url::parse(url.as_str()).unwrap())
            .build()
    };
    let view_a = make_view(a.clone());
    let view_b = make_view(b.clone());
    let view_a2 = make_view(a.clone());
    for (view, value) in [(&view_a, "A"), (&view_b, "B")] {
        evaluate_javascript(&test, view.clone(), format!(
            "localStorage.setItem('profile', '{value}'); sessionStorage.setItem('profile', '{value}'); document.cookie = 'profile={value}; path=/';"
        )).unwrap();
    }
    for (view, value) in [(&view_a, "A"), (&view_a2, "A"), (&view_b, "B")] {
        assert_eq!(
            evaluate_javascript(
                &test,
                view.clone(),
                "localStorage.profile + ':' + document.cookie"
            )
            .unwrap(),
            JSValue::String(format!("{value}:profile={value}"))
        );
    }
    assert_eq!(
        evaluate_javascript(&test, view_a2.clone(), "sessionStorage.getItem('profile')").unwrap(),
        JSValue::Null
    );

    // Storage notifications and BroadcastChannel must have the same boundary as the data.
    for view in [&view_a2, &view_b] {
        evaluate_javascript(&test, view.clone(), "window.events = []; onstorage = e => events.push(e.newValue); window.bc = new BroadcastChannel('partition'); bc.onmessage = e => events.push(e.data);").unwrap();
    }
    evaluate_javascript(&test, view_a.clone(), "localStorage.profile = 'changed'; window.bc = new BroadcastChannel('partition'); bc.postMessage('message');").unwrap();
    assert_eq!(
        partition_async_result(
            &test,
            &view_a2,
            "while (events.length < 2) await new Promise(r => setTimeout(r, 10)); return events.sort().join(',');"
        ),
        "changed,message"
    );
    assert_eq!(
        evaluate_javascript(&test, view_b.clone(), "events.length").unwrap(),
        JSValue::Number(0.0)
    );

    assert_eq!(
        partition_async_result(
            &test,
            &view_a,
            "window.lockReady = false; navigator.locks.request('profile-lock', () => { window.lockReady = true; return new Promise(resolve => window.releaseLock = resolve); }); while (!window.lockReady) await new Promise(r => setTimeout(r, 10)); return 'held';"
        ),
        "held"
    );
    assert_eq!(
        partition_async_result(
            &test,
            &view_b,
            "return await navigator.locks.request('profile-lock', {ifAvailable: true}, lock => !!lock);"
        ),
        "true"
    );
    assert_eq!(
        partition_async_result(
            &test,
            &view_a2,
            "return await navigator.locks.request('profile-lock', {ifAvailable: true}, lock => !!lock);"
        ),
        "false"
    );
    evaluate_javascript(&test, view_a.clone(), "releaseLock();").unwrap();
    // about:srcdoc inherits storage, but Servo treats its non-network document
    // URL as cookie-averse. Check cookies on a fetch from that child instead.
    assert_eq!(
        partition_async_result(
            &test,
            &view_b,
            "const frame = document.createElement('iframe'); frame.srcdoc = '<!doctype html>Child'; const ready = new Promise(resolve => frame.onload = resolve); document.body.append(frame); await ready; const cookie = await (await frame.contentWindow.fetch('/cookie')).text(); return frame.contentWindow.localStorage.profile + ':' + cookie;"
        ),
        "B:profile=B"
    );

    for (view, value) in [(&view_a, "A"), (&view_b, "B")] {
        assert_eq!(
            partition_async_result(
                &test,
                view,
                "const answer = new Promise(resolve => window.addEventListener('message', e => resolve(e.data), {once: true})); const frame = document.createElement('iframe'); frame.sandbox = 'allow-scripts'; frame.src = '/sandbox'; document.body.append(frame); return await answer;"
            ),
            format!("profile={value}")
        );
    }

    struct PopupDelegate {
        context: Rc<dyn RenderingContext>,
        popup: RefCell<Option<WebView>>,
    }
    impl WebViewDelegate for PopupDelegate {
        fn request_create_new(&self, _: WebView, request: CreateNewWebViewRequest) {
            self.popup
                .replace(Some(request.builder(self.context.clone()).build()));
        }
    }
    let delegate = Rc::new(PopupDelegate {
        context: test.rendering_context.clone(),
        popup: RefCell::new(None),
    });
    let opener = WebViewBuilder::new(test.servo(), test.rendering_context.clone())
        .storage_partition(a.clone())
        .delegate(delegate.clone())
        .url(Url::parse(url.as_str()).unwrap())
        .build();
    evaluate_javascript(
        &test,
        opener.clone(),
        "sessionStorage.profile = 'opener'; window.open('about:blank');",
    )
    .unwrap();
    test.spin(|| delegate.popup.borrow().is_none());
    let popup = delegate.popup.borrow().as_ref().unwrap().clone();
    assert_eq!(popup.storage_partition().id(), a.id());
    // Check inherited storage before navigating the cookie-averse about:blank.
    assert_eq!(
        evaluate_javascript(
            &test,
            popup.clone(),
            "localStorage.profile + ':' + sessionStorage.profile"
        )
        .unwrap(),
        JSValue::String("changed:opener".into())
    );
    popup.load(Url::parse(url.as_str()).unwrap());
    test.spin(|| {
        popup
            .url()
            .is_none_or(|current| current.as_str() != url.as_str())
    });
    assert_eq!(
        evaluate_javascript(&test, popup, "localStorage.profile + ':' + document.cookie").unwrap(),
        JSValue::String("changed:profile=A".into())
    );
    delegate.popup.borrow_mut().take();
    evaluate_javascript(
        &test,
        opener.clone(),
        "window.open('/', '_blank', 'noopener');",
    )
    .unwrap();
    test.spin(|| delegate.popup.borrow().is_none());
    let no_opener = delegate.popup.borrow().as_ref().unwrap().clone();
    assert_eq!(no_opener.storage_partition().id(), a.id());
    assert_eq!(
        evaluate_javascript(
            &test,
            no_opener,
            "String(opener) + ':' + localStorage.profile"
        )
        .unwrap(),
        JSValue::String("null:changed".into())
    );
    server.close();
}

#[test]
fn test_partition_indexeddb_cache_and_shared_worker_owners() {
    let test = ServoTest::new_with_builder(|builder| {
        let mut preferences = Preferences::default();
        preferences.dom_serviceworker_enabled = true;
        builder.preferences(preferences)
    });
    let (server, url) = make_server(|request, response| {
        if request.uri().path() == "/worker.js" {
            response.headers_mut().insert(
                http::header::CONTENT_TYPE,
                HeaderValue::from_static("text/javascript"),
            );
            *response.body_mut() = make_body(b"let count = 0; onconnect = event => { const port = event.ports[0]; port.onmessage = async () => { await navigator.locks.query(); port.postMessage(++count); }; port.start(); };".to_vec());
        } else {
            *response.body_mut() = make_body(b"<!doctype html>Partitions".to_vec());
        }
    });
    let a = test.servo().create_ephemeral_storage_partition().unwrap();
    let b = test.servo().create_ephemeral_storage_partition().unwrap();
    let make_view = |partition| {
        WebViewBuilder::new(test.servo(), test.rendering_context.clone())
            .storage_partition(partition)
            .url(Url::parse(url.as_str()).unwrap())
            .build()
    };
    let view_a = make_view(a.clone());
    let view_b = make_view(b);
    let view_a2 = make_view(a);
    for (view, value) in [(&view_a, "A"), (&view_b, "B")] {
        assert_eq!(
            partition_async_result(
                &test,
                view,
                &format!(
                    r#"
            const db = await new Promise((resolve, reject) => {{
                const request = indexedDB.open('same-database', 1);
                request.onupgradeneeded = () => request.result.createObjectStore('values');
                request.onsuccess = () => resolve(request.result);
                request.onerror = () => reject(request.error);
            }});
            await new Promise((resolve, reject) => {{
                const tx = db.transaction('values', 'readwrite');
                tx.objectStore('values').put('{value}', 'profile');
                tx.oncomplete = resolve; tx.onerror = () => reject(tx.error);
            }});
            db.close();
            const cache = await caches.open('same-cache');
            return 'written';
        "#
                )
            ),
            "written"
        );
    }
    for (view, value) in [(&view_a2, "A"), (&view_b, "B")] {
        assert_eq!(
            partition_async_result(
                &test,
                view,
                r#"
            const db = await new Promise((resolve, reject) => {
                const request = indexedDB.open('same-database', 1);
                request.onsuccess = () => resolve(request.result); request.onerror = () => reject(request.error);
            });
            const value = await new Promise((resolve, reject) => {
                const request = db.transaction('values').objectStore('values').get('profile');
                request.onsuccess = () => resolve(request.result); request.onerror = () => reject(request.error);
            });
            db.close(); return value;
        "#
            ),
            value
        );
    }
    assert_eq!(
        partition_async_result(
            &test,
            &view_a2,
            "window.heldDatabase = await new Promise((resolve, reject) => { const request = indexedDB.open('same-database', 1); request.onsuccess = () => resolve(request.result); request.onerror = () => reject(request.error); }); return 'held';"
        ),
        "held"
    );
    assert_eq!(
        partition_async_result(
            &test,
            &view_a,
            "window.upgradeComplete = false; return await new Promise((resolve, reject) => { const request = indexedDB.open('same-database', 2); request.onblocked = () => resolve('blocked'); request.onerror = () => reject(request.error); request.onsuccess = () => { request.result.close(); window.upgradeComplete = true; }; });"
        ),
        "blocked"
    );
    assert_eq!(
        partition_async_result(
            &test,
            &view_b,
            "return await new Promise((resolve, reject) => { const request = indexedDB.open('same-database', 2); request.onblocked = () => reject(new Error('another partition blocked this upgrade')); request.onerror = () => reject(request.error); request.onsuccess = () => { request.result.close(); resolve('upgraded'); }; });"
        ),
        "upgraded"
    );
    evaluate_javascript(&test, view_a2.clone(), "heldDatabase.close();").unwrap();
    assert_eq!(
        partition_async_result(
            &test,
            &view_a,
            "while (!upgradeComplete) await new Promise(r => setTimeout(r, 10)); return 'upgraded';"
        ),
        "upgraded"
    );

    partition_async_result(
        &test,
        &view_a,
        "await caches.delete('same-cache'); return 'deleted';",
    );
    assert_eq!(
        partition_async_result(&test, &view_b, "return await caches.has('same-cache');"),
        "true"
    );
    assert_eq!(
        partition_async_result(&test, &view_a2, "return await caches.has('same-cache');"),
        "false"
    );

    let connect = "window.worker = new SharedWorker('/worker.js', 'same-name'); worker.port.start(); return await new Promise(resolve => { worker.port.onmessage = e => resolve(e.data); worker.port.postMessage('next'); });";
    assert_eq!(partition_async_result(&test, &view_a, connect), "1");
    assert_eq!(partition_async_result(&test, &view_b, connect), "1");
    assert_eq!(partition_async_result(&test, &view_a2, connect), "2");
    drop(view_a);
    assert_eq!(
        partition_async_result(
            &test,
            &view_a2,
            "return await new Promise(resolve => { worker.port.onmessage = e => resolve(e.data); worker.port.postMessage('next'); });"
        ),
        "3"
    );
    server.close();
}

#[test]
fn test_serviceworker_scope_is_partitioned() {
    let test = ServoTest::new_with_builder(|builder| {
        let mut preferences = Preferences::default();
        preferences.dom_serviceworker_enabled = true;
        builder.preferences(preferences)
    });
    let (server, url) = make_server(|request, response| {
        if request.uri().path() == "/sw.js" {
            let value = if request.uri().query() == Some("A") {
                "A"
            } else {
                "B"
            };
            response.headers_mut().insert(
                http::header::CONTENT_TYPE,
                HeaderValue::from_static("text/javascript"),
            );
            *response.body_mut() = make_body(format!(r#"
                self.onmessage = event => {{
                    const port = event.ports[0];
                    if (event.data === 'pending') {{
                        const open = indexedDB.open('pending-shutdown', 1);
                        open.onerror = () => port.postMessage('ERROR: ' + open.error);
                        open.onsuccess = () => {{
                            self.heldDatabase = open.result;
                            const upgrade = indexedDB.open('pending-shutdown', 2);
                            upgrade.onblocked = () => port.postMessage('blocked');
                            upgrade.onerror = () => port.postMessage('ERROR: ' + upgrade.error);
                            upgrade.onsuccess = () => port.postMessage('ERROR: upgrade was not blocked');
                        }};
                        return;
                    }}
                    (async () => {{
                        const own = await caches.has('owner-{value}');
                        const other = await caches.has('owner-' + ('{value}' === 'A' ? 'B' : 'A'));
                        const cookie = await (await fetch('/cookie')).text();
                        return '{value}:' + own + ':' + other + ':' + cookie;
                    }})().then(value => port.postMessage(value), error => port.postMessage('ERROR: ' + error));
                }};
            "#).into_bytes());
        } else if request.uri().path() == "/cookie" {
            *response.body_mut() = make_body(
                request
                    .headers()
                    .get(http::header::COOKIE)
                    .map_or_else(Vec::new, |cookie| cookie.as_bytes().to_vec()),
            );
        } else {
            *response.body_mut() = make_body(b"<!doctype html>Partition service workers".to_vec());
        }
    });
    let root = tempfile::tempdir().unwrap();
    let a = test
        .servo()
        .create_storage_partition(root.path().join("A"))
        .unwrap();
    let b = test.servo().create_ephemeral_storage_partition().unwrap();
    let make_view = |partition| {
        WebViewBuilder::new(test.servo(), test.rendering_context.clone())
            .storage_partition(partition)
            .url(Url::parse(url.as_str()).unwrap())
            .build()
    };
    let view_a = make_view(a);
    let view_b = make_view(b);
    for (view, value) in [(&view_a, "A"), (&view_b, "B")] {
        assert_eq!(
            partition_async_result(
                &test,
                view,
                &format!(
                    r#"
            if (!isSecureContext) throw new Error('service worker test requires a secure context');
            if (!navigator.serviceWorker) throw new Error('service worker API is disabled');
            document.cookie = 'profile={value}; path=/';
            await caches.open('owner-{value}');
            window.registration = await navigator.serviceWorker.register('/sw.js?{value}', {{scope: '/'}});
            window.partitionWorker = registration.installing || registration.waiting || registration.active;
            if (!partitionWorker) throw new Error('registration returned no worker');
            window.askWorker = command => new Promise((resolve, reject) => {{
                const channel = new MessageChannel();
                const timer = setTimeout(() => {{ channel.port1.close(); reject(new Error('service worker reply timed out: ' + command)); }}, 4000);
                channel.port1.onmessage = event => {{ clearTimeout(timer); channel.port1.close(); resolve(event.data); }};
                partitionWorker.postMessage(command, [channel.port2]);
            }});
            return await askWorker('profile');
        "#
                )
            ),
            format!("{value}:true:false:profile={value}")
        );
    }
    // This revision installs workers but does not implement activation. Exercise
    // registration/message routing again after both same-scope workers exist.
    // The worker's outgoing fetch and Cache Storage query use its own handles.
    for (view, value) in [(&view_a, "A"), (&view_b, "B")] {
        assert_eq!(
            partition_async_result(&test, view, "return await askWorker('profile');"),
            format!("{value}:true:false:profile={value}")
        );
    }
    // Leave live workers with blocked IDB upgrades for multi-partition shutdown.
    for view in [&view_a, &view_b] {
        assert_eq!(
            partition_async_result(&test, view, "return await askWorker('pending');"),
            "blocked"
        );
    }
    // Queue navigation before close while the workers still own blocked requests.
    view_a.load(Url::parse(url.as_str()).unwrap().join("/pending").unwrap());
    view_b.load(Url::parse(url.as_str()).unwrap().join("/pending").unwrap());
    drop(view_a);
    drop(view_b);
    server.close();
    drop(test);
}

#[test]
fn test_popup_rejects_conflicting_partition() {
    let test = ServoTest::new();
    let a = test.servo().create_ephemeral_storage_partition().unwrap();
    let b = test.servo().create_ephemeral_storage_partition().unwrap();
    struct RejectPopup {
        servo: servo::Servo,
        context: Rc<dyn RenderingContext>,
        other: servo::StoragePartition,
        rejected: Cell<bool>,
    }
    impl WebViewDelegate for RejectPopup {
        fn request_create_new(&self, _: WebView, request: CreateNewWebViewRequest) {
            let _registered_during_callback =
                self.servo.create_ephemeral_storage_partition().unwrap();
            let result = request
                .builder(self.context.clone())
                .storage_partition(self.other.clone())
                .try_build();
            self.rejected.set(matches!(
                result,
                Err(servo::StoragePartitionError::PopupPartitionConflict)
            ));
        }
    }
    let delegate = Rc::new(RejectPopup {
        servo: test.servo().clone(),
        context: test.rendering_context.clone(),
        other: b,
        rejected: Cell::new(false),
    });
    let view = WebViewBuilder::new(test.servo(), test.rendering_context.clone())
        .storage_partition(a)
        .delegate(delegate.clone())
        .build();
    assert_eq!(
        evaluate_javascript(&test, view, "window.open('about:blank') === null").unwrap(),
        JSValue::Boolean(true)
    );
    assert!(delegate.rejected.get());
}

fn check_partition_web_fonts(canvas_backend: &str) {
    let test = ServoTest::new_with_builder(|builder| {
        let mut preferences = Preferences::default();
        preferences.dom_canvas_backend = canvas_backend.into();
        preferences.dom_fontface_enabled = true;
        builder.preferences(preferences)
    });
    let (server, url) = make_server(|request, response| {
        if request.uri().path() == "/profile-font.ttf" {
            let is_a = request
                .headers()
                .get(http::header::COOKIE)
                .is_some_and(|cookie| cookie.to_str().unwrap().contains("profile=A"));
            let bytes: &[u8] = if is_a {
                include_bytes!("../../../tests/wpt/tests/fonts/Ahem.ttf")
            } else {
                include_bytes!("../../../tests/wpt/tests/fonts/Lato-Medium.ttf")
            };
            response.headers_mut().insert(
                http::header::CONTENT_TYPE,
                HeaderValue::from_static("font/ttf"),
            );
            response.headers_mut().insert(
                http::header::CACHE_CONTROL,
                HeaderValue::from_static("public, max-age=3600"),
            );
            *response.body_mut() = make_body(bytes.to_vec());
        } else {
            *response.body_mut() = make_body(b"<!doctype html>Partition fonts".to_vec());
        }
    });
    let a = test.servo().create_ephemeral_storage_partition().unwrap();
    let b = test.servo().create_ephemeral_storage_partition().unwrap();
    for (partition, value) in [(&a, "A"), (&b, "B")] {
        partition.site_data_manager().set_cookie_for_url(
            Url::parse(url.as_str()).unwrap(),
            cookie::Cookie::new("profile", value),
            None,
        );
    }
    let make_view = |partition| {
        WebViewBuilder::new(test.servo(), test.rendering_context.clone())
            .storage_partition(partition)
            .url(Url::parse(url.as_str()).unwrap())
            .build()
    };
    let view_a = make_view(a.clone());
    let view_b = make_view(b);
    let view_a2 = make_view(a);
    let draw = r#"
        const face = await new FontFace('PartitionFont', 'url(/profile-font.ttf)').load();
        document.fonts.add(face);
        const canvas = document.createElement('canvas'); canvas.width = 200; canvas.height = 100;
        const context = canvas.getContext('2d'); context.font = '24px PartitionFont';
        context.fillText('WWiii', 0, 30); context.strokeText('WWiii', 0, 70);
        return canvas.toDataURL();
    "#;
    let image_a = partition_async_result(&test, &view_a, draw);
    let image_b = partition_async_result(&test, &view_b, draw);
    let image_a2 = partition_async_result(&test, &view_a2, draw);
    assert_ne!(
        image_a, image_b,
        "same URL reused another partition's font bytes"
    );
    assert_eq!(image_a, image_a2, "same partition font rendering changed");
    server.close();
}

#[test]
fn test_partition_web_fonts_vello_cpu() {
    check_partition_web_fonts("vello_cpu");
}

#[cfg(feature = "vello")]
#[test]
fn test_partition_web_fonts_vello_gpu() {
    check_partition_web_fonts("vello");
}

#[test]
fn test_partition_survives_cross_origin_navigation_iframes_and_history() {
    let test = ServoTest::new();
    replace_host_table(HashMap::from([
        ("partition-a.test".into(), "127.0.0.1".parse().unwrap()),
        ("partition-b.test".into(), "127.0.0.1".parse().unwrap()),
    ]));
    let (server, url) = make_server(|request, response| {
        let body = if request.uri().path() == "/child" {
            "<!doctype html><script>parent.postMessage(localStorage.getItem('profile'), '*')</script>"
        } else {
            "<!doctype html>Partition history"
        };
        *response.body_mut() = make_body(body.as_bytes().to_vec());
    });
    let first = Url::parse(&format!("http://partition-a.test:{}/", url.port().unwrap())).unwrap();
    let second = Url::parse(&format!("http://partition-b.test:{}/", url.port().unwrap())).unwrap();
    let partitions = [
        test.servo().create_ephemeral_storage_partition().unwrap(),
        test.servo().create_ephemeral_storage_partition().unwrap(),
    ];
    let views = partitions.each_ref().map(|partition| {
        WebViewBuilder::new(test.servo(), test.rendering_context.clone())
            .storage_partition(partition.clone())
            .url(first.clone())
            .build()
    });
    for (index, view) in views.iter().enumerate() {
        evaluate_javascript(
            &test,
            view.clone(),
            format!("localStorage.profile = 'first-{index}'"),
        )
        .unwrap();
        view.load(second.clone());
        test.spin(|| view.url().as_ref() != Some(&second));
        evaluate_javascript(
            &test,
            view.clone(),
            format!("localStorage.profile = 'second-{index}'"),
        )
        .unwrap();
        view.go_back(1);
        test.spin(|| view.url().as_ref() != Some(&first));
        assert_eq!(
            evaluate_javascript(&test, view.clone(), "localStorage.profile").unwrap(),
            JSValue::String(format!("first-{index}"))
        );
        view.reload();
        assert_eq!(
            evaluate_javascript(&test, view.clone(), "localStorage.profile").unwrap(),
            JSValue::String(format!("first-{index}"))
        );
        assert_eq!(
            partition_async_result(
                &test,
                view,
                &format!(
                    r#"
            const answer = new Promise(resolve => window.addEventListener('message', e => resolve(e.data), {{once: true}}));
            const frame = document.createElement('iframe'); frame.src = '{second}child'; document.body.append(frame);
            return await answer;
        "#
                )
            ),
            format!("second-{index}")
        );
        view.go_forward(1);
        test.spin(|| view.url().as_ref() != Some(&second));
        assert_eq!(
            evaluate_javascript(&test, view.clone(), "localStorage.profile").unwrap(),
            JSValue::String(format!("second-{index}"))
        );
        assert_eq!(view.storage_partition().id(), partitions[index].id());
    }
    server.close();
}

#[test]
fn test_partition_persistent_storage_reopens_in_a_fresh_servo_process() {
    // Servo's process-wide JS/runtime state requires a fresh process for restart
    // coverage. Reuse this test binary and the existing HTTP fixture.
    let Ok(root) = std::env::var("SERVO_PARTITION_REOPEN_ROOT") else {
        let root = tempfile::tempdir().unwrap();
        let (server, url) = make_server(|_, response| {
            *response.body_mut() = make_body(b"<!doctype html>Persistent partitions".to_vec());
        });
        for phase in ["write", "read"] {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "test_partition_persistent_storage_reopens_in_a_fresh_servo_process",
                    "--nocapture",
                ])
                .env("SERVO_PARTITION_REOPEN_ROOT", root.path())
                .env("SERVO_PARTITION_REOPEN_URL", url.as_str())
                .env("SERVO_PARTITION_REOPEN_PHASE", phase)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{phase} process failed:\n{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        server.close();
        return;
    };
    let url = Url::parse(&std::env::var("SERVO_PARTITION_REOPEN_URL").unwrap()).unwrap();
    let write = std::env::var("SERVO_PARTITION_REOPEN_PHASE").unwrap() == "write";
    let test = ServoTest::new();
    let partitions = ["A", "B"].map(|name| {
        test.servo()
            .create_storage_partition(std::path::Path::new(&root).join(name))
            .unwrap()
    });
    let views = partitions.each_ref().map(|partition| {
        WebViewBuilder::new(test.servo(), test.rendering_context.clone())
            .storage_partition(partition.clone())
            .url(url.clone())
            .build()
    });
    for (index, view) in views.iter().enumerate() {
        if write {
            assert_eq!(
                partition_async_result(
                    &test,
                    view,
                    &format!(
                        r#"
                localStorage.profile = '{index}'; document.cookie = 'profile={index}; Max-Age=3600; path=/';
                const db = await new Promise((resolve, reject) => {{
                    const request = indexedDB.open('durable', 1);
                    request.onupgradeneeded = () => request.result.createObjectStore('values');
                    request.onsuccess = () => resolve(request.result); request.onerror = () => reject(request.error);
                }});
                await new Promise((resolve, reject) => {{
                    const tx = db.transaction('values', 'readwrite'); tx.objectStore('values').put('{index}', 'profile');
                    tx.oncomplete = resolve; tx.onerror = () => reject(tx.error);
                }});
                db.close(); return 'written';
            "#
                    )
                ),
                "written"
            );
        } else {
            assert_eq!(
                partition_async_result(
                    &test,
                    view,
                    &format!(
                        r#"
                const db = await new Promise((resolve, reject) => {{
                    const request = indexedDB.open('durable', 1);
                    request.onupgradeneeded = () => reject(new Error('durable database missing'));
                    request.onsuccess = () => resolve(request.result); request.onerror = () => reject(request.error);
                }});
                const value = await new Promise((resolve, reject) => {{
                    const request = db.transaction('values').objectStore('values').get('profile');
                    request.onsuccess = () => resolve(request.result); request.onerror = () => reject(request.error);
                }});
                db.close();
                return localStorage.profile + ':' + document.cookie + ':' + value;
            "#
                    )
                ),
                format!("{index}:profile={index}:{index}")
            );
        }
    }
    drop(views);
    drop(test);
}

#[test]
fn test_ephemeral_apis_and_shutdown_leave_configured_profile_untouched() {
    let root = tempfile::tempdir().unwrap();
    let files = [
        "cookie_jar.json",
        "auth_cache.json",
        "hsts_list.json",
        "http-cache/cache.sqlite3",
        "webstorage/webstorage.sqlite",
        "clientstorage/default_v1/reg.sqlite",
        "cachestorage/default_v1/sentinel",
    ];
    for file in files {
        let path = root.path().join(file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, b"persistent profile must remain unchanged").unwrap();
    }
    let directories = [
        "",
        "http-cache",
        "webstorage",
        "clientstorage",
        "clientstorage/default_v1",
        "cachestorage",
        "cachestorage/default_v1",
    ];
    let snapshot = || {
        directories.map(|directory| {
            let mut entries: Vec<_> = std::fs::read_dir(root.path().join(directory))
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect();
            entries.sort();
            entries
        })
    };
    let before = snapshot();
    let test = ServoTest::new_with_builder(|builder| {
        let mut preferences = Preferences::default();
        preferences.dom_serviceworker_enabled = true;
        preferences.network_http_disk_cache = root
            .path()
            .join("http-cache/cache.sqlite3")
            .to_string_lossy()
            .into_owned();
        builder
            .opts(servo::Opts {
                config_dir: Some(root.path().to_owned()),
                temporary_storage: true,
                ..Default::default()
            })
            .preferences(preferences)
    });
    let (server, url) = make_server(|_, response| {
        *response.body_mut() = make_body(b"<!doctype html>Ephemeral partitions".to_vec());
    });
    let partitions = [
        test.servo().default_storage_partition(),
        test.servo().create_ephemeral_storage_partition().unwrap(),
    ];
    for partition in &partitions {
        assert!(partition.root().is_none());
        let view = WebViewBuilder::new(test.servo(), test.rendering_context.clone())
            .storage_partition(partition.clone())
            .url(Url::parse(url.as_str()).unwrap())
            .build();
        assert_eq!(
            partition_async_result(
                &test,
                &view,
                r#"
            localStorage.profile = 'temporary'; sessionStorage.profile = 'temporary'; document.cookie = 'profile=temporary; path=/';
            const db = await new Promise((resolve, reject) => {
                const request = indexedDB.open('temporary', 1);
                request.onupgradeneeded = () => request.result.createObjectStore('values');
                request.onsuccess = () => resolve(request.result); request.onerror = () => reject(request.error);
            });
            await new Promise((resolve, reject) => {
                const tx = db.transaction('values', 'readwrite'); tx.objectStore('values').put('temporary', 'profile');
                tx.oncomplete = resolve; tx.onerror = () => reject(tx.error);
            });
            db.close(); await caches.open('temporary'); await fetch('/network'); return 'written';
        "#
            ),
            "written"
        );
    }
    server.close();
    drop(test);
    // Keep partition handles alive while verifying that shutdown did not persist their data.
    assert_eq!(before, snapshot());
    for file in files {
        assert_eq!(
            std::fs::read(root.path().join(file)).unwrap(),
            b"persistent profile must remain unchanged"
        );
    }
}

mod navigation_action_tests {
    use std::time::{Duration, Instant};

    use http::Method;
    use servo::{
        Code, Key, KeyState, KeyboardEvent, Location, Modifiers, NamedKey, NavigationAction,
        NavigationRequest, NavigationType, WebViewId,
    };

    use super::*;

    struct NavigationDelegate {
        requests: RefCell<Vec<(WebViewId, Url, NavigationAction)>>,
        creations: RefCell<Vec<(Url, NavigationAction)>>,
        popups: RefCell<Vec<WebView>>,
        context: Rc<dyn RenderingContext>,
        allow_creation: Cell<bool>,
    }

    impl NavigationDelegate {
        fn new(test: &ServoTest) -> Rc<Self> {
            Rc::new(Self {
                requests: RefCell::default(),
                creations: RefCell::default(),
                popups: RefCell::default(),
                context: test.rendering_context.clone(),
                allow_creation: Cell::new(true),
            })
        }

        fn take_request(&self, test: &ServoTest) -> (WebViewId, Url, NavigationAction) {
            wait_until(test, || !self.requests.borrow().is_empty());
            assert_eq!(self.requests.borrow().len(), 1);
            self.requests.borrow_mut().pop().unwrap()
        }
    }

    impl WebViewDelegate for NavigationDelegate {
        fn notify_new_frame_ready(&self, webview: WebView) {
            webview.paint();
        }

        fn request_navigation(&self, webview: WebView, request: NavigationRequest) {
            // Allow setup of the iframe; deny test navigations so the page stays available.
            if request.url.path() == "/frame" {
                request.allow();
                return;
            }
            self.requests.borrow_mut().push((
                webview.id(),
                request.url.clone(),
                request.action.clone(),
            ));
            request.deny();
        }

        fn request_create_new(&self, parent: WebView, request: CreateNewWebViewRequest) {
            self.creations
                .borrow_mut()
                .push((request.url.clone(), request.action.clone()));
            if self.allow_creation.get() {
                let popup = request
                    .builder(self.context.clone())
                    .delegate(parent.delegate())
                    .build();
                self.popups.borrow_mut().push(popup);
            }
        }
    }

    fn wait_until(test: &ServoTest, ready: impl Fn() -> bool) {
        let start = Instant::now();
        test.spin(|| {
            assert!(
                start.elapsed() < Duration::from_secs(30),
                "navigation action timed out"
            );
            !ready()
        });
    }

    fn page(
        test: &ServoTest,
        delegate: &Rc<NavigationDelegate>,
    ) -> (net::test_util::Server, WebView) {
        let (server, url) = make_server(|request, response| {
            response.headers_mut().insert(
                http::header::CONTENT_TYPE,
                HeaderValue::from_static("text/html"),
            );
            let html = if request.uri().path() == "/frame" && request.uri().query() == Some("top") {
                "<!doctype html><style>body { margin: 0 } a { display: block; width: 100px; height: 80px }</style><a href='/destination' target='_top'>Top link</a>"
            } else if request.uri().path() == "/frame" {
                "<!doctype html><style>body { margin: 0 } a { display: block; width: 100px; height: 80px }</style><a href='/destination'>Frame link</a>"
            } else {
                "<!doctype html><style>body { margin: 0 } a, button { display: block; width: 100px; height: 80px } iframe { position: absolute; left: 0; top: 200px; width: 100px; height: 80px; border: 0 }</style>\
                 <a id='link' href='/destination'>Link</a>\
                 <form action='/destination' method='post'><input type='hidden' name='value' value='body'><button type='submit'>Submit</button></form>\
                 <iframe src='/frame'></iframe>"
            };
            *response.body_mut() = make_body(html.as_bytes().to_vec());
        });
        let view = WebViewBuilder::new(test.servo(), test.rendering_context.clone())
            .delegate(delegate.clone())
            .url(Url::parse(url.as_str()).unwrap())
            .build();
        view.show();
        wait_until(test, || view.load_status() == LoadStatus::Complete);
        wait_for_webview_scene_to_be_up_to_date(test, &view);
        assert!(delegate.requests.borrow().is_empty());
        (server, view)
    }

    fn key(view: &WebView, state: KeyState, key: NamedKey, code: Code, modifiers: Modifiers) {
        view.notify_input_event(InputEvent::Keyboard(KeyboardEvent::new_without_event(
            state,
            Key::Named(key),
            code,
            Location::Standard,
            modifiers,
            false,
            false,
        )));
    }

    fn assert_action(
        action: &NavigationAction,
        kind: NavigationType,
        main_frame: bool,
        modifiers: Modifiers,
        method: Method,
    ) {
        assert_eq!(action.navigation_type, kind);
        assert_eq!(action.is_main_frame, main_frame);
        assert_eq!(action.modifiers, modifiers);
        assert_eq!(action.method, method);
    }

    #[test]
    fn test_link_iframe_form_and_script_navigation_actions() {
        let test = ServoTest::new();
        let delegate = NavigationDelegate::new(&test);
        let (server, view) = page(&test, &delegate);

        for (point, kind, main_frame, method) in [
            (
                DevicePoint::new(20., 20.),
                NavigationType::LinkActivated,
                true,
                Method::GET,
            ),
            (
                DevicePoint::new(20., 220.),
                NavigationType::LinkActivated,
                false,
                Method::GET,
            ),
            (
                DevicePoint::new(20., 120.),
                NavigationType::FormSubmitted,
                true,
                Method::POST,
            ),
        ] {
            click_at_point(&view, point, MouseButton::Primary);
            let (id, url, action) = delegate.take_request(&test);
            assert_eq!(id, view.id());
            assert_eq!(url.path(), "/destination");
            assert_action(&action, kind, main_frame, Modifiers::empty(), method);
        }

        evaluate_javascript(&test, view.clone(), "location.href = '/destination'").unwrap();
        let (_, _, action) = delegate.take_request(&test);
        assert_action(
            &action,
            NavigationType::Other,
            true,
            Modifiers::empty(),
            Method::GET,
        );

        evaluate_javascript(
            &test,
            view.clone(),
            "document.querySelector('form').submit()",
        )
        .unwrap();
        let (_, _, action) = delegate.take_request(&test);
        assert_action(
            &action,
            NavigationType::FormSubmitted,
            true,
            Modifiers::empty(),
            Method::POST,
        );

        evaluate_javascript(&test, view.clone(), "document.querySelector('a').click()").unwrap();
        let (_, _, action) = delegate.take_request(&test);
        assert_action(
            &action,
            NavigationType::Other,
            true,
            Modifiers::empty(),
            Method::GET,
        );

        view.reload();
        let (_, _, action) = delegate.take_request(&test);
        assert_action(
            &action,
            NavigationType::Reload,
            true,
            Modifiers::empty(),
            Method::GET,
        );
        server.close();
    }

    #[test]
    fn test_shift_mouse_and_keyboard_navigation_actions() {
        let test = ServoTest::new();
        let delegate = NavigationDelegate::new(&test);
        let (server, view) = page(&test, &delegate);

        key(
            &view,
            KeyState::Down,
            NamedKey::Shift,
            Code::ShiftLeft,
            Modifiers::empty(),
        );
        click_at_point(&view, DevicePoint::new(20., 20.), MouseButton::Primary);
        let (_, _, action) = delegate.take_request(&test);
        assert_action(
            &action,
            NavigationType::LinkActivated,
            true,
            Modifiers::SHIFT,
            Method::GET,
        );

        // A script navigation while Shift remains held has no activating event.
        evaluate_javascript(&test, view.clone(), "location.href = '/destination'").unwrap();
        let (_, _, action) = delegate.take_request(&test);
        assert_action(
            &action,
            NavigationType::Other,
            true,
            Modifiers::empty(),
            Method::GET,
        );

        // The keydown's default action synthesizes a click. It must retain Shift.
        evaluate_javascript(&test, view.clone(), "document.querySelector('a').focus()").unwrap();
        key(
            &view,
            KeyState::Down,
            NamedKey::Enter,
            Code::Enter,
            Modifiers::SHIFT,
        );
        let (_, _, action) = delegate.take_request(&test);
        assert_action(
            &action,
            NavigationType::LinkActivated,
            true,
            Modifiers::SHIFT,
            Method::GET,
        );
        key(
            &view,
            KeyState::Up,
            NamedKey::Enter,
            Code::Enter,
            Modifiers::SHIFT,
        );

        click_at_point(&view, DevicePoint::new(20., 120.), MouseButton::Primary);
        let (_, _, action) = delegate.take_request(&test);
        assert_action(
            &action,
            NavigationType::FormSubmitted,
            true,
            Modifiers::SHIFT,
            Method::POST,
        );
        // Enter in a text control submits through the editing-action path.
        evaluate_javascript(
            &test,
            view.clone(),
            r#"
            document.querySelector('form').insertAdjacentHTML('beforeend', '<input id="entry">');
            document.querySelector('#entry').focus();
        "#,
        )
        .unwrap();
        key(
            &view,
            KeyState::Down,
            NamedKey::Enter,
            Code::Enter,
            Modifiers::SHIFT,
        );
        let (_, _, action) = delegate.take_request(&test);
        assert_action(
            &action,
            NavigationType::FormSubmitted,
            true,
            Modifiers::SHIFT,
            Method::POST,
        );
        key(
            &view,
            KeyState::Up,
            NamedKey::Enter,
            Code::Enter,
            Modifiers::SHIFT,
        );
        key(
            &view,
            KeyState::Up,
            NamedKey::Shift,
            Code::ShiftLeft,
            Modifiers::SHIFT,
        );
        server.close();
    }

    #[test]
    fn test_auxiliary_navigation_action_matches_first_load() {
        let test = ServoTest::new();
        let delegate = NavigationDelegate::new(&test);
        let (server, view) = page(&test, &delegate);

        // Cover both target=_blank and the platform's alternate link action.
        for target_blank in [true, false] {
            evaluate_javascript(
                &test,
                view.clone(),
                if target_blank {
                    "link.target = '_blank'"
                } else {
                    "link.removeAttribute('target')"
                },
            )
            .unwrap();
            let modifiers = if target_blank {
                Modifiers::SHIFT
            } else if cfg!(target_os = "macos") {
                Modifiers::SHIFT | Modifiers::META
            } else {
                Modifiers::SHIFT | Modifiers::CONTROL
            };
            key(
                &view,
                KeyState::Down,
                NamedKey::Shift,
                Code::ShiftLeft,
                modifiers,
            );
            click_at_point(&view, DevicePoint::new(20., 20.), MouseButton::Primary);
            let (id, url, action) = delegate.take_request(&test);
            let (creation_url, creation_action) = delegate.creations.borrow_mut().pop().unwrap();
            assert_eq!(creation_url, url);
            assert_eq!(creation_action, action);
            assert_ne!(id, view.id());
            assert_eq!(id, delegate.popups.borrow().last().unwrap().id());
            assert_action(
                &action,
                NavigationType::LinkActivated,
                true,
                modifiers,
                Method::GET,
            );
            key(
                &view,
                KeyState::Up,
                NamedKey::Shift,
                Code::ShiftLeft,
                Modifiers::SHIFT,
            );

            // These popups share the opener's RenderingContext and overlap it.
            // Hit testing uses the shared WebRender scene, so an already painted
            // about:blank popup can intercept the next click sent through `view`.
            // Keep the popup alive for the assertions, but remove it from the scene
            // and wait for that change before clicking the opener again.
            delegate.popups.borrow().last().unwrap().hide();
            wait_for_webview_scene_to_be_up_to_date(&test, &view);
        }

        // The embedder can inspect Shift+_blank and decline before a WebView exists.
        delegate.allow_creation.set(false);
        evaluate_javascript(&test, view.clone(), "link.target = '_blank'").unwrap();
        key(
            &view,
            KeyState::Down,
            NamedKey::Shift,
            Code::ShiftLeft,
            Modifiers::empty(),
        );
        click_at_point(&view, DevicePoint::new(20., 20.), MouseButton::Primary);
        wait_until(&test, || !delegate.creations.borrow().is_empty());
        let (_, action) = delegate.creations.borrow_mut().pop().unwrap();
        assert_action(
            &action,
            NavigationType::LinkActivated,
            true,
            Modifiers::SHIFT,
            Method::GET,
        );
        assert_eq!(delegate.popups.borrow().len(), 2);
        assert!(delegate.requests.borrow().is_empty());
        key(
            &view,
            KeyState::Up,
            NamedKey::Shift,
            Code::ShiftLeft,
            Modifiers::SHIFT,
        );
        server.close();
    }

    #[test]
    fn test_cross_origin_iframe_link_targets_main_frame() {
        let test = ServoTest::new();
        let delegate = NavigationDelegate::new(&test);
        let (server, view) = page(&test, &delegate);
        // localhost and 127.0.0.1 are distinct origins. The iframe's _top target
        // exercises routing to a destination owned by another script thread.
        evaluate_javascript(
            &test,
            view.clone(),
            r#"
            window.frameReady = false;
            const frame = document.querySelector('iframe');
            frame.onload = () => window.frameReady = true;
            const url = new URL('/frame?top', location.href);
            url.hostname = '127.0.0.1';
            frame.src = url.href;
        "#,
        )
        .unwrap();
        wait_until(&test, || {
            evaluate_javascript(&test, view.clone(), "window.frameReady").unwrap() ==
                JSValue::Boolean(true)
        });
        wait_for_webview_scene_to_be_up_to_date(&test, &view);
        key(
            &view,
            KeyState::Down,
            NamedKey::Shift,
            Code::ShiftLeft,
            Modifiers::empty(),
        );
        click_at_point(&view, DevicePoint::new(20., 220.), MouseButton::Primary);
        let (id, url, action) = delegate.take_request(&test);
        assert_eq!(id, view.id());
        assert_eq!(url.host_str(), Some("127.0.0.1"));
        assert_action(
            &action,
            NavigationType::LinkActivated,
            true,
            Modifiers::SHIFT,
            Method::GET,
        );
        key(
            &view,
            KeyState::Up,
            NamedKey::Shift,
            Code::ShiftLeft,
            Modifiers::SHIFT,
        );
        server.close();
    }
}
