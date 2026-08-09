/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use embedder_traits::{HitTestResult, WebViewPoint};
use euclid::Point2D;
use html5ever::LocalName;
use js::context::JSContext;
use script_bindings::num::Finite;
use style_traits::CSSPixel;

use crate::document_layout_snapshot::{attribute_value, stamp_document_handles};
use crate::dom::bindings::codegen::Bindings::DocumentBinding::DocumentMethods;
use crate::dom::bindings::codegen::Bindings::WindowBinding::WindowMethods;
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::root::DomRoot;
use crate::dom::element::Element;
use crate::dom::node::Node;
use crate::dom::node::iterators::ShadowIncluding;
use crate::dom::window::Window;

/// Answer the topmost event-receiving element at `point` in `window`'s active document.
///
/// This is the same paint-tree query Servo's WebDriver `element_click` performs through
/// `DocumentOrShadowRoot::elements_from_point`: shadow-root results are retargeted to the
/// document, consecutive duplicates are removed, and the root element is the fallback,
/// all in front-to-back paint order. The embedder receives the topmost element's
/// document-stable handle rather than a boolean, so it can decide whether the element it
/// intends to actuate is actually on top.
pub(crate) fn query(window: &Window, point: WebViewPoint, cx: &mut JSContext) -> HitTestResult {
    // Mirror the snapshot seam's handle assignment so the returned handle always matches
    // the `DocumentLayoutSnapshotNode::handle` a D1 capture reports for the same element.
    // Stamping is idempotent and deterministic in document order, and the elements-from-
    // point query below forces a reflow, so the stamped attributes are part of the
    // laid-out state it answers against.
    let document = window.Document();
    let elements: Vec<DomRoot<Element>> = document
        .upcast::<Node>()
        .traverse_preorder(ShadowIncluding::No)
        .filter_map(DomRoot::downcast)
        .collect();
    stamp_document_handles(cx, &elements);

    // Device pixels -> CSS viewport pixels with the live viewport scale. Page-pixel
    // points round-trip through the same scale, and the DOM query works in CSS viewport
    // coordinates either way.
    let viewport = window.viewport_details();
    let point: Point2D<f32, CSSPixel> =
        point.as_device_point(viewport.hidpi_scale_factor) / viewport.hidpi_scale_factor;

    let paint_tree = document.ElementsFromPoint(
        Finite::wrap(point.x as f64),
        Finite::wrap(point.y as f64),
    );

    // An empty paint tree covers both out-of-viewport points and points with no painted
    // element, mirroring `DocumentOrShadowRoot::elements_from_point` returning an empty
    // sequence for both.
    let Some(topmost) = paint_tree.first() else {
        return HitTestResult::Outside;
    };

    // Stamping above guarantees the attribute exists; the fallback is defensive.
    match attribute_value(topmost, &LocalName::from("data-theorem-id")) {
        Some(handle) if !handle.trim().is_empty() => HitTestResult::Handle(handle),
        _ => HitTestResult::Outside,
    }
}
