/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */
//! Bounded native semantics for actual current-document controls and read-only
//! evidence. IDs are Node-owned, and actions re-read the node and layout state.
use crate::dom::bindings::codegen::UnionTypes::BooleanOrScrollIntoViewOptions;
use crate::dom::bindings::inheritance::Castable;
use crate::dom::iterators::ShadowIncluding;
use crate::dom::node::{Node, NodeTraits};
use crate::dom::types::{
    Element, HTMLInputElement, HTMLSelectElement, HTMLTextAreaElement, Window,
};
use js::context::JSContext;
use script_bindings::codegen::GenericBindings::{
    ElementBinding::{ElementMethods, ScrollIntoViewOptions, ScrollLogicalPosition},
    HTMLInputElementBinding::HTMLInputElementMethods,
    HTMLSelectElementBinding::HTMLSelectElementMethods,
    HTMLTextAreaElementBinding::HTMLTextAreaElementMethods,
    NodeBinding::NodeMethods,
    WindowBinding::{ScrollBehavior, ScrollOptions, WindowMethods},
};
use servo_base::native_accessibility::*;
use servo_base::native_text::{NativeTextAction, NativeTextError, NativeTextRequest};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use style::computed_values::visibility::T as Visibility;
use web_atoms::{LocalName, local_name};
const MAX_NODES: usize = 1024;
const MAX_TEXT: usize = 8192;
fn attr(element: &Element, name: &str) -> Option<String> {
    element.get_attribute_string_value(&LocalName::from(name))
}
fn bounded(mut value: String, limit: usize, truncated: &mut bool) -> String {
    if value.len() > limit {
        let mut end = limit;
        while !value.is_char_boundary(end) {
            end -= 1;
        }
        value.truncate(end);
        *truncated = true;
    }
    value
}
fn role(element: &Element) -> Option<&'static str> {
    match element.local_name().as_ref() {
        "button" | "summary" => Some("button"),
        "select" => Some("combobox"),
        "textarea" => Some("text_input"),
        "input" => match attr(element, "type").as_deref().unwrap_or("text") {
            "hidden" => None,
            "checkbox" => Some("checkbox"),
            "radio" => Some("radio"),
            "button" | "submit" | "reset" => Some("button"),
            "number" | "range" => Some("spin_button"),
            _ => Some("text_input"),
        },
        "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => Some("heading"),
        "pre" | "p" | "label" | "output" => Some("text"),
        "ul" | "ol" => Some("list"),
        "li" => Some("list_item"),
        _ => None,
    }
}
fn text(node: &Node) -> String {
    node.GetTextContent()
        .map(|text| text.to_string())
        .unwrap_or_default()
}
fn label(element: &Element) -> String {
    if let Some(label) = attr(element, "aria-label").filter(|v| !v.is_empty()) {
        return label;
    }
    if let Some(label) = attr(element, "title").filter(|v| !v.is_empty()) {
        return label;
    }
    let node = element.upcast::<Node>();
    if matches!(
        element.local_name().as_ref(),
        "input" | "select" | "textarea"
    ) {
        if let Some(label) = node.ancestors().find_map(|node| {
            let element = node.downcast::<Element>()?;
            (element.local_name() == &local_name!("label")).then(|| text(&node))
        }) {
            return label;
        }
    }
    text(node)
}
fn observe(
    window: &Window,
    element: &Element,
    parent: Option<String>,
    truncated: &mut bool,
) -> Option<NativeAccessibilityNode> {
    let role = role(element)?;
    let node = element.upcast::<Node>();
    if node
        .inclusive_ancestors(ShadowIncluding::No)
        .any(|ancestor| {
            ancestor.downcast::<Element>().is_some_and(|e| {
                e.has_attribute(&local_name!("hidden"))
                    || e.has_attribute(&local_name!("inert"))
                    || attr(e, "aria-hidden").as_deref() == Some("true")
            })
        })
    {
        return None;
    }
    if element
        .style()
        .is_none_or(|style| style.get_inherited_box().visibility != Visibility::Visible)
    {
        return None;
    }
    let rect = node.border_box()?;
    let viewport = window.viewport_details().size;
    let x = rect.origin.x.to_f64_px().max(0.);
    let y = rect.origin.y.to_f64_px().max(0.);
    let right = (rect.origin.x + rect.size.width)
        .to_f64_px()
        .min(viewport.width as f64);
    let bottom = (rect.origin.y + rect.size.height)
        .to_f64_px()
        .min(viewport.height as f64);
    let raw = [
        rect.origin.x.to_f64_px(),
        rect.origin.y.to_f64_px(),
        rect.size.width.to_f64_px(),
        rect.size.height.to_f64_px(),
    ];
    if raw.iter().any(|v| !v.is_finite()) || raw[2] <= 0. || raw[3] <= 0. {
        return None;
    }
    let offscreen = right <= x || bottom <= y;
    let point = (!offscreen).then_some([(x + right) / 2., (y + bottom) / 2.]);
    // Current hit testing excludes covered viewport controls. Offscreen rendered
    // nodes expose only reveal/focus and bounded text reads until scrolled in.
    if let Some(point) = point {
        let hit = window
            .hit_test_from_point_in_viewport(euclid::point2(point[0] as f32, point[1] as f32))?;
        if !node.is_shadow_including_inclusive_ancestor_of(&hit.node) {
            return None;
        }
    }
    let disabled = element.is_actually_disabled();
    let readonly = role == "text" || element.has_attribute(&local_name!("readonly"));
    let protected = element.local_name() == &local_name!("input")
        && attr(element, "type").as_deref() == Some("password");
    let mut selection = None;
    let mut editable = false;
    let value = if let Some(input) = element.downcast::<HTMLInputElement>() {
        if let Some((_, range)) = input.native_text_snapshot() {
            selection = Some(range);
            editable = true;
        }
        Some(input.Value().to_string())
    } else if let Some(area) = element.downcast::<HTMLTextAreaElement>() {
        if let Some((_, range)) = area.native_text_snapshot() {
            selection = Some(range);
            editable = true;
        }
        Some(area.Value().to_string())
    } else if let Some(select) = element.downcast::<HTMLSelectElement>() {
        Some(select.Value().to_string())
    } else if role == "text" {
        Some(text(node))
    } else {
        None
    };
    let text_utf16_length = value
        .as_ref()
        .map(|value| value.encode_utf16().count())
        .and_then(|len| u32::try_from(len).ok());
    let text_digest = value
        .as_ref()
        .map(|value| format!("{:x}", Sha256::digest(value.as_bytes())));
    let mut actions = Vec::new();
    if offscreen {
        actions.push(NativeAccessibilityActionKind::Reveal);
    }
    if value.is_some() && !protected {
        actions.push(NativeAccessibilityActionKind::ReadText);
    }
    if !disabled
        && matches!(
            role,
            "button" | "combobox" | "text_input" | "spin_button" | "checkbox" | "radio"
        )
    {
        actions.push(NativeAccessibilityActionKind::Focus);
        if !offscreen {
            actions.push(NativeAccessibilityActionKind::Click);
        }
        if editable && !readonly && !protected {
            actions.push(NativeAccessibilityActionKind::SetValue);
            actions.push(NativeAccessibilityActionKind::SelectText);
        }
    }
    Some(NativeAccessibilityNode {
        id: node.unique_id(window.pipeline_id()),
        parent,
        revision: node.inclusive_descendants_version().to_string(),
        text_digest,
        text_utf16_length,
        offscreen,
        protected,
        role: role.into(),
        label: bounded(label(element), 4096, truncated),
        value: if protected {
            None
        } else {
            value.map(|value| bounded(value, MAX_TEXT, truncated))
        },
        bounds: if offscreen {
            raw
        } else {
            [x, y, right - x, bottom - y]
        },
        action_point: point,
        disabled,
        readonly,
        focused: element.focus_state(),
        selected: if matches!(role, "checkbox" | "radio") {
            element
                .downcast::<HTMLInputElement>()
                .map(|input| input.Checked())
        } else {
            attr(element, "aria-selected").map(|v| v == "true")
        },
        expanded: if element.local_name() == &local_name!("summary") {
            node.GetParentElement()
                .map(|parent| parent.has_attribute(&local_name!("open")))
        } else {
            attr(element, "aria-expanded").map(|v| v == "true")
        },
        selection,
        actions,
    })
}
fn snapshot(window: &Window) -> NativeAccessibilityResult {
    let document = window.Document();
    if !document.is_fully_active() {
        return Err(NativeTextError::DocumentUnavailable);
    }
    let mut nodes = Vec::new();
    let mut known = HashSet::new();
    let mut truncated = false;
    let mut total = 0;
    for node in document
        .upcast::<Node>()
        .traverse_preorder(ShadowIncluding::No)
    {
        let Some(element) = node.downcast::<Element>() else {
            continue;
        };
        let parent = node.ancestors().find_map(|parent| {
            let id = parent.unique_id(window.pipeline_id());
            known.contains(&id).then_some(id)
        });
        if let Some(observation) = observe(window, element, parent, &mut truncated) {
            total += observation.label.len() + observation.value.as_ref().map_or(0, String::len);
            if nodes.len() >= MAX_NODES || total > 262144 {
                truncated = true;
                break;
            }
            known.insert(observation.id.clone());
            nodes.push(observation);
        }
    }
    let viewport = window.viewport_details().size;
    Ok(NativeAccessibilitySnapshot {
        webview: window.webview_id(),
        document: window.pipeline_id(),
        viewport: [viewport.width as f64, viewport.height as f64],
        nodes,
        truncated,
        text: None,
    })
}
pub(crate) fn dispatch(
    window: &Window,
    request: NativeAccessibilityRequest,
    cx: &mut JSContext,
) -> NativeAccessibilityResult {
    if matches!(request, NativeAccessibilityRequest::Identity) {
        if !window.Document().is_fully_active() {
            return Err(NativeTextError::DocumentUnavailable);
        }
        let viewport = window.viewport_details().size;
        return Ok(NativeAccessibilitySnapshot {
            webview: window.webview_id(),
            document: window.pipeline_id(),
            viewport: [viewport.width as f64, viewport.height as f64],
            nodes: Vec::new(),
            truncated: false,
            text: None,
        });
    }
    let current = snapshot(window)?;
    let NativeAccessibilityRequest::Action {
        webview,
        document,
        expected,
        action,
    } = request
    else {
        return Ok(current);
    };
    if current.webview != webview
        || current.document != document
        || !current.nodes.iter().any(|node| node == &expected)
    {
        return Err(NativeTextError::StaleContext);
    }
    let kind = match &action {
        NativeAccessibilityAction::Focus => NativeAccessibilityActionKind::Focus,
        NativeAccessibilityAction::Reveal => NativeAccessibilityActionKind::Reveal,
        NativeAccessibilityAction::ReadText { .. } => NativeAccessibilityActionKind::ReadText,
        NativeAccessibilityAction::Click => NativeAccessibilityActionKind::Click,
        NativeAccessibilityAction::SetValue { .. } => NativeAccessibilityActionKind::SetValue,
        NativeAccessibilityAction::SelectText { .. } => NativeAccessibilityActionKind::SelectText,
    };
    if !expected.actions.contains(&kind) {
        return Err(NativeTextError::Unsupported);
    }
    let document = window.Document();
    let node = document
        .upcast::<Node>()
        .traverse_preorder(ShadowIncluding::No)
        .find(|node| node.unique_id(window.pipeline_id()) == expected.id)
        .ok_or(NativeTextError::StaleContext)?;
    match action {
        NativeAccessibilityAction::ReadText { mut range } => {
            let value = if let Some(input) = node.downcast::<HTMLInputElement>() {
                input.Value().to_string()
            } else if let Some(area) = node.downcast::<HTMLTextAreaElement>() {
                area.Value().to_string()
            } else if let Some(select) = node.downcast::<HTMLSelectElement>() {
                select.Value().to_string()
            } else {
                text(&node)
            };
            let units: Vec<_> = value.encode_utf16().collect();
            if range.0 > range.1 || range.1 as usize > units.len() || range.1 - range.0 > 8192 {
                return Err(NativeTextError::InvalidRange);
            }
            // Page boundaries are UTF-16 units; preserve whole scalar values at
            // either boundary instead of publishing a broken surrogate pair.
            if range.0 > 0
                && range.0 < units.len() as u32
                && (0xDC00..=0xDFFF).contains(&units[range.0 as usize])
            {
                range.0 -= 1;
            }
            if range.1 > range.0
                && range.1 < units.len() as u32
                && (0xDC00..=0xDFFF).contains(&units[range.1 as usize])
            {
                range.1 -= 1;
            }
            while range.1 - range.0 > 8192 {
                range.1 -= 1;
            }
            if range.1 > range.0
                && range.1 < units.len() as u32
                && (0xDC00..=0xDFFF).contains(&units[range.1 as usize])
            {
                range.1 -= 1;
            }
            let value = String::from_utf16(&units[range.0 as usize..range.1 as usize])
                .map_err(|_| NativeTextError::InvalidRange)?;
            let mut result = current;
            result.text = Some(NativeAccessibilityText {
                node: expected.id,
                revision: expected.revision,
                digest: expected.text_digest.ok_or(NativeTextError::Unsupported)?,
                range,
                total_utf16_length: units.len() as u32,
                value,
            });
            return Ok(result);
        },
        NativeAccessibilityAction::Reveal => {
            let element = node
                .downcast::<Element>()
                .ok_or(NativeTextError::Unsupported)?;
            reveal(element, cx);
        },
        NativeAccessibilityAction::Click => {
            let point = expected.action_point.ok_or(NativeTextError::EditRefused)?;
            window
                .Document()
                .event_handler()
                .handle_native_accessibility_click(
                    cx,
                    euclid::point2(point[0] as f32, point[1] as f32),
                    &node,
                )?;
        },
        action => {
            let element = node
                .downcast::<Element>()
                .ok_or(NativeTextError::Unsupported)?;
            if expected.offscreen {
                reveal(element, cx);
            }
            if !node.run_the_focusing_steps(cx, None) {
                return Err(NativeTextError::EditRefused);
            }
            if !element.focus_state() {
                return Err(NativeTextError::StaleContext);
            }
            if !window.Document().is_fully_active() {
                return Err(NativeTextError::DocumentUnavailable);
            }
            if !matches!(action, NativeAccessibilityAction::Focus) {
                let context =
                    crate::native_text::snapshot(window)?.ok_or(NativeTextError::EditRefused)?;
                if context.element != expected.id
                    || expected.text_digest.as_deref()
                        != Some(format!("{:x}", Sha256::digest(context.text.as_bytes())).as_str())
                {
                    return Err(NativeTextError::StaleContext);
                }
                let action = match action {
                    NativeAccessibilityAction::SetValue { value } => NativeTextAction::Replace {
                        range: Some((0, context.text.encode_utf16().count() as u32)),
                        text: value,
                    },
                    NativeAccessibilityAction::SelectText { range } => {
                        NativeTextAction::Select { range }
                    },
                    _ => unreachable!(),
                };
                crate::native_text::dispatch(
                    window,
                    NativeTextRequest::Edit {
                        expected: context,
                        action,
                    },
                    cx,
                )?;
            }
        },
    }
    snapshot(window)
}

fn reveal(element: &Element, cx: &mut JSContext) {
    element.ScrollIntoView(
        cx,
        BooleanOrScrollIntoViewOptions::ScrollIntoViewOptions(ScrollIntoViewOptions {
            parent: ScrollOptions {
                behavior: ScrollBehavior::Instant,
            },
            block: ScrollLogicalPosition::End,
            inline: ScrollLogicalPosition::Nearest,
            container: Default::default(),
        }),
    );
}
