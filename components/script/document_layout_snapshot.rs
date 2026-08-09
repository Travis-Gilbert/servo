/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::collections::{BTreeMap, HashSet};

use embedder_traits::{
    DocumentLayoutComputedStyle, DocumentLayoutRect, DocumentLayoutSnapshot,
    DocumentLayoutSnapshotError, DocumentLayoutSnapshotNode, DocumentLayoutSnapshotNodeSource,
    DocumentLayoutSnapshotSource, DocumentLayoutViewport, UntrustedNodeAddress,
};
use euclid::Rect;
use html5ever::{LocalName, local_name, ns};
use js::context::JSContext;
use layout_api::{
    DocumentLayoutSnapshotComputedStyle, DocumentLayoutSnapshotProjectionNode, QueryMsg, ReflowGoal,
};
use rustc_hash::FxHashMap;
use style::attr::AttrValue;
use style_traits::CSSPixel;

use crate::dom::bindings::codegen::Bindings::DocumentBinding::DocumentMethods;
use crate::dom::bindings::codegen::Bindings::HTMLInputElementBinding::HTMLInputElementMethods;
use crate::dom::bindings::codegen::Bindings::HTMLSelectElementBinding::HTMLSelectElementMethods;
use crate::dom::bindings::codegen::Bindings::HTMLTextAreaElementBinding::HTMLTextAreaElementMethods;
use crate::dom::bindings::codegen::Bindings::NodeBinding::NodeMethods;
use crate::dom::bindings::codegen::Bindings::WindowBinding::WindowMethods;
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::root::DomRoot;
use crate::dom::element::Element;
use crate::dom::html::htmlcanvaselement::HTMLCanvasElement;
use crate::dom::html::htmlselectelement::HTMLSelectElement;
use crate::dom::html::htmltextareaelement::HTMLTextAreaElement;
use crate::dom::html::input_element::HTMLInputElement;
use crate::dom::node::Node;
use crate::dom::node::iterators::ShadowIncluding;
use crate::dom::window::Window;

pub(crate) fn capture(
    window: &Window,
    cx: &mut JSContext,
) -> Result<DocumentLayoutSnapshot, DocumentLayoutSnapshotError> {
    let document = window.Document();
    let elements: Vec<DomRoot<Element>> = document
        .upcast::<Node>()
        .traverse_preorder(ShadowIncluding::No)
        .filter_map(DomRoot::downcast)
        .collect();

    stamp_document_handles(cx, &elements);

    let canvas_fallback_by_node = canvas_fallback_by_node(&elements);

    // Stamping is a real DOM mutation and data-attribute selectors may affect style. Force a
    // query reflow after stamping, then read both the FragmentTree and stacking-context tree from
    // that same completed layout generation.
    window.reflow(
        cx,
        ReflowGoal::LayoutQuery(QueryMsg::DocumentLayoutSnapshot),
    );
    let projection = window
        .layout()
        .query_document_layout_snapshot()
        .ok_or(DocumentLayoutSnapshotError::LayoutUnavailable)?;
    let layout_by_node: FxHashMap<UntrustedNodeAddress, DocumentLayoutSnapshotProjectionNode> =
        projection
            .nodes
            .into_iter()
            .map(|node| (node.node, node))
            .collect();

    let nodes = elements
        .iter()
        .map(|element| build_node(cx, element, &layout_by_node, &canvas_fallback_by_node))
        .collect();
    let viewport_details = window.viewport_details();

    Ok(DocumentLayoutSnapshot {
        url: document.url().to_string(),
        title: document.Title().str().to_owned(),
        nodes,
        device_pixel_ratio: viewport_details.hidpi_scale_factor.get(),
        viewport: DocumentLayoutViewport {
            width: physical_dimension(viewport_details.device_size.width),
            height: physical_dimension(viewport_details.device_size.height),
        },
        // The fragment-tree projection is the only capture path in the engine;
        // the embedder's injected-script fallback records its own source.
        source: DocumentLayoutSnapshotSource::FragmentTree,
    })
}

pub(crate) fn stamp_document_handles(cx: &mut JSContext, elements: &[DomRoot<Element>]) {
    let mut used = HashSet::new();
    let mut next = 0_u64;

    for element in elements {
        let existing = attribute_value(element, &LocalName::from("data-theorem-id"))
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty());
        let handle = existing
            .as_ref()
            .filter(|value| used.insert((*value).clone()))
            .cloned()
            .unwrap_or_else(|| {
                loop {
                    let candidate = format!("t{next}");
                    next = next.saturating_add(1);
                    if used.insert(candidate.clone()) {
                        break candidate;
                    }
                }
            });

        if existing.as_deref() != Some(handle.as_str()) {
            element.set_attribute(
                cx,
                &LocalName::from("data-theorem-id"),
                AttrValue::String(handle),
            );
        }
    }
}

/// Map every element that is declared fallback content of a `<canvas>` (a DOM
/// descendant of the canvas element, i.e. between its tags) to that canvas
/// element's node address. Canvas is a replaced box: its children never reach
/// the `FragmentTree`, so fallback nodes take their bounds from the canvas
/// element's own layout record.
fn canvas_fallback_by_node(
    elements: &[DomRoot<Element>],
) -> FxHashMap<UntrustedNodeAddress, UntrustedNodeAddress> {
    elements
        .iter()
        .filter_map(|element| {
            let node = element.upcast::<Node>();
            let mut ancestor = node.GetParentElement();
            while let Some(candidate) = ancestor {
                if candidate.is::<HTMLCanvasElement>() {
                    return Some((
                        node.to_untrusted_node_address(),
                        candidate.upcast::<Node>().to_untrusted_node_address(),
                    ));
                }
                ancestor = candidate.upcast::<Node>().GetParentElement();
            }
            None
        })
        .collect()
}

fn build_node(
    cx: &mut JSContext,
    element: &Element,
    layout_by_node: &FxHashMap<UntrustedNodeAddress, DocumentLayoutSnapshotProjectionNode>,
    canvas_fallback_by_node: &FxHashMap<UntrustedNodeAddress, UntrustedNodeAddress>,
) -> DocumentLayoutSnapshotNode {
    let node = element.upcast::<Node>();
    let layout = layout_by_node.get(&node.to_untrusted_node_address());
    let canvas_fallback = canvas_fallback_by_node.get(&node.to_untrusted_node_address());
    // Fallback content has no fragments of its own; carry the enclosing canvas's
    // layout record so its bounds are the canvas element's own.
    let canvas_layout = canvas_fallback.and_then(|canvas| layout_by_node.get(canvas));
    let handle = attribute_value(element, &LocalName::from("data-theorem-id"))
        .expect("document handles were stamped before layout capture");
    let parent = node
        .GetParentElement()
        .and_then(|parent| attribute_value(&parent, &LocalName::from("data-theorem-id")));
    let tag = element.local_name().to_string().to_ascii_lowercase();
    let attributes = attributes_of(element);
    let role = role_of(element, &tag);
    let name = name_of(element);
    let test_id = ["data-testid", "data-test-id", "data-test"]
        .into_iter()
        .find_map(|name| attribute_value(element, &LocalName::from(name)));
    let text = non_empty(&node.child_text_content().str());

    DocumentLayoutSnapshotNode {
        handle,
        parent,
        tag,
        role,
        name,
        value: value_of(cx, element),
        test_id,
        attributes,
        bbox: canvas_layout
            .and_then(|canvas| canvas.bbox.map(public_rect))
            .or_else(|| layout.and_then(|layout| layout.bbox.map(public_rect))),
        client_rect: canvas_layout
            .and_then(|canvas| canvas.client_rect.map(public_rect))
            .or_else(|| layout.and_then(|layout| layout.client_rect.map(public_rect))),
        scroll_rect: layout.and_then(|layout| layout.scroll_rect.map(public_rect)),
        paint_order: layout.and_then(|layout| layout.paint_order),
        stacking_context: layout.and_then(|layout| layout.stacking_context),
        computed: layout
            .map(|layout| public_computed(layout.computed.clone()))
            .unwrap_or_default(),
        visible: canvas_layout
            .map(|canvas| canvas.visible)
            .or_else(|| layout.map(|layout| layout.visible))
            .unwrap_or(false),
        enabled: !element.is_actually_disabled()
            && !attribute_value(element, &local_name!("aria-disabled"))
                .is_some_and(|value| value.eq_ignore_ascii_case("true")),
        editable: element.read_write_state(),
        scrollable: layout.is_some_and(|layout| layout.scrollable),
        text,
        source: if canvas_fallback.is_some() {
            DocumentLayoutSnapshotNodeSource::CanvasFallback
        } else {
            DocumentLayoutSnapshotNodeSource::Dom
        },
    }
}

fn role_of(element: &Element, tag: &str) -> String {
    if let Some(role) = attribute_value(element, &local_name!("role")) {
        return role.to_ascii_lowercase();
    }
    match tag {
        "a" if element.has_attribute(&local_name!("href")) => "link".to_owned(),
        "button" => "button".to_owned(),
        "textarea" => "textbox".to_owned(),
        "select" => "select".to_owned(),
        "input" => attribute_value(element, &local_name!("type"))
            .unwrap_or_else(|| "text".to_owned())
            .to_ascii_lowercase(),
        _ => tag.to_owned(),
    }
}

fn name_of(element: &Element) -> String {
    let mut parts: Vec<String> = ["aria-label", "title", "placeholder", "alt"]
        .into_iter()
        .filter_map(|name| attribute_value(element, &LocalName::from(name)))
        .filter(|value| !value.is_empty())
        .collect();
    let text = element.upcast::<Node>().descendant_text_content();
    if let Some(text) = non_empty(&text.str()) {
        parts.push(text);
    }
    parts.join(" ").trim().to_owned()
}

fn value_of(cx: &JSContext, element: &Element) -> Option<String> {
    if let Some(input) = element.downcast::<HTMLInputElement>() {
        return Some(input.Value().str().to_owned());
    }
    if let Some(textarea) = element.downcast::<HTMLTextAreaElement>() {
        return Some(textarea.Value().str().to_owned());
    }
    if let Some(select) = element.downcast::<HTMLSelectElement>() {
        return Some(select.Value(cx).str().to_owned());
    }
    None
}

fn attributes_of(element: &Element) -> BTreeMap<String, String> {
    element
        .attrs()
        .borrow()
        .iter()
        .map(|attribute| {
            (
                attribute.name().to_string(),
                attribute.to_dom_string().str().to_owned(),
            )
        })
        .collect()
}

pub(crate) fn attribute_value(element: &Element, name: &LocalName) -> Option<String> {
    element.with_attribute(&ns!(), name, |attribute| {
        attribute.to_dom_string().str().to_owned()
    })
}

fn non_empty(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_owned())
}

fn public_rect(rect: Rect<i32, CSSPixel>) -> DocumentLayoutRect {
    DocumentLayoutRect {
        x: rect.origin.x,
        y: rect.origin.y,
        width: rect.size.width,
        height: rect.size.height,
    }
}

fn public_computed(computed: DocumentLayoutSnapshotComputedStyle) -> DocumentLayoutComputedStyle {
    DocumentLayoutComputedStyle {
        display: computed.display,
        visibility: computed.visibility,
        position: computed.position,
        overflow_x: computed.overflow_x,
        overflow_y: computed.overflow_y,
        opacity: computed.opacity,
        pointer_events: computed.pointer_events,
        cursor: computed.cursor,
        white_space: computed.white_space,
        font_size: computed.font_size,
    }
}

fn physical_dimension(value: f32) -> u32 {
    if value.is_finite() {
        value.max(0.0).round() as u32
    } else {
        0
    }
}
