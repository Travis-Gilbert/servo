/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */
//! Native text admission re-reads actual focused-document state. Cached embedder
//! InputMethodControl notifications are never an edit authority.
use crate::dom::bindings::inheritance::Castable;
use crate::dom::node::{Node, NodeTraits};
use crate::dom::types::{HTMLInputElement, HTMLTextAreaElement, Window};
use embedder_traits::{ImeEvent, InputEventResult};
use js::context::JSContext;
use keyboard_types::{CompositionEvent, CompositionState};
use script_bindings::codegen::GenericBindings::WindowBinding::WindowMethods;
use servo_base::native_text::*;
use std::cell::RefCell;

#[derive(Default, JSTraceable, MallocSizeOf)]
pub(crate) struct State {
    #[no_trace]
    #[ignore_malloc_size_of = "bounded native text context; no DOM refs"]
    marked: RefCell<Option<NativeTextContext>>,
}
fn snapshot(window: &Window) -> NativeTextResult {
    let document = window.Document();
    if !document.is_fully_active() {
        return Err(NativeTextError::DocumentUnavailable);
    }
    let area = document.focus_handler().focused_area();
    let Some(element) = area.element() else {
        return Ok(None);
    };
    let buffer = if let Some(input) = element.downcast::<HTMLInputElement>() {
        input.native_text_snapshot()
    } else if let Some(input) = element.downcast::<HTMLTextAreaElement>() {
        input.native_text_snapshot()
    } else {
        None
    };
    let Some((text, selection)) = buffer else {
        return Ok(None);
    };
    if text.len() > 65536 {
        return Err(NativeTextError::TooLarge);
    }
    let node = element.upcast::<Node>();
    let Some(rect) = node.border_box() else {
        return Ok(None);
    };
    let mut context = NativeTextContext {
        webview: window.webview_id(),
        document: window.pipeline_id(),
        element: node.unique_id(window.pipeline_id()),
        focus_sequence: document.focus_handler().focus_sequence().0,
        text,
        selection,
        marked: None,
        rect: [
            rect.origin.x.to_f64_px(),
            rect.origin.y.to_f64_px(),
            rect.size.width.to_f64_px(),
            rect.size.height.to_f64_px(),
        ],
    };
    let state = window.native_text().marked.borrow();
    if let Some(previous) = state.as_ref() {
        // A script/key/focus change invalidates the marked range instead of relabeling it.
        if previous.webview == context.webview
            && previous.document == context.document
            && previous.element == context.element
            && previous.focus_sequence == context.focus_sequence
            && previous.text == context.text
            && previous.selection == context.selection
        {
            context.marked = previous.marked;
        }
    }
    Ok(Some(context))
}
fn valid_range(text: &str, range: (u32, u32)) -> bool {
    if range.0 > range.1 {
        return false;
    }
    let mut offset = 0u32;
    let mut start = range.0 == 0;
    let mut end = range.1 == 0;
    for ch in text.chars() {
        offset += ch.len_utf16() as u32;
        start |= offset == range.0;
        end |= offset == range.1;
    }
    start && end
}
fn select(window: &Window, range: (u32, u32)) -> Result<(), NativeTextError> {
    let document = window.Document();
    let area = document.focus_handler().focused_area();
    let element = area.element().ok_or(NativeTextError::StaleContext)?;
    if let Some(input) = element.downcast::<HTMLInputElement>() {
        input.native_text_select(range);
    } else if let Some(input) = element.downcast::<HTMLTextAreaElement>() {
        input.native_text_select(range);
    } else {
        return Err(NativeTextError::Unsupported);
    }
    Ok(())
}
fn composition(
    window: &Window,
    cx: &mut JSContext,
    state: CompositionState,
    text: String,
) -> InputEventResult {
    window.Document().event_handler().handle_ime_event(
        cx,
        ImeEvent::Composition(CompositionEvent { state, data: text }),
    )
}
pub(crate) fn dispatch(
    window: &Window,
    request: NativeTextRequest,
    cx: &mut JSContext,
) -> NativeTextResult {
    let current = snapshot(window)?;
    let NativeTextRequest::Edit { expected, action } = request else {
        return Ok(current);
    };
    if current.as_ref() != Some(&expected) {
        return Err(NativeTextError::StaleContext);
    }
    match action {
        NativeTextAction::Select { range } => {
            if !valid_range(&expected.text, range) {
                return Err(NativeTextError::InvalidRange);
            }
            select(window, range)?;
            window.native_text().marked.borrow_mut().take();
        },
        NativeTextAction::Unmark => {
            if expected.marked.is_some() {
                composition(window, cx, CompositionState::End, String::new());
            }
            window.native_text().marked.borrow_mut().take();
        },
        action => {
            let (range, text, preedit, selection) = match action {
                NativeTextAction::Replace { range, text } => (range, text, false, None),
                NativeTextAction::Preedit {
                    range,
                    text,
                    selection,
                } => (range, text, true, selection),
                _ => unreachable!(),
            };
            if text.len() > 65536 || expected.text.len().saturating_add(text.len()) > 131072 {
                return Err(NativeTextError::TooLarge);
            }
            let range = range.or(expected.marked).unwrap_or(expected.selection);
            if !valid_range(&expected.text, range)
                || selection.is_some_and(|selection| !valid_range(&text, selection))
            {
                return Err(NativeTextError::InvalidRange);
            }
            if preedit && expected.marked.is_none() {
                if composition(window, cx, CompositionState::Start, String::new())
                    .contains(InputEventResult::DefaultPrevented)
                {
                    return Err(NativeTextError::EditRefused);
                }
                // Native callbacks can run author handlers; never apply to their replacement focus/value.
                if snapshot(window)?.as_ref() != Some(&expected) {
                    return Err(NativeTextError::StaleContext);
                }
            }
            let mut expected_text: Vec<u16> = expected.text.encode_utf16().collect();
            expected_text.splice(range.0 as usize..range.1 as usize, text.encode_utf16());
            let expected_text =
                String::from_utf16(&expected_text).map_err(|_| NativeTextError::InvalidRange)?;
            select(window, range)?;
            if text.is_empty() && range.0 != range.1 {
                let document = window.Document();
                let area = document.focus_handler().focused_area();
                let element = area.element().ok_or(NativeTextError::StaleContext)?;
                if let Some(input) = element.downcast::<HTMLInputElement>() {
                    input.native_text_delete_selection(cx, preedit);
                } else if let Some(input) = element.downcast::<HTMLTextAreaElement>() {
                    input.native_text_delete_selection(cx, preedit);
                }
            }
            composition(
                window,
                cx,
                if preedit {
                    CompositionState::Update
                } else {
                    CompositionState::End
                },
                text.clone(),
            );
            window.native_text().marked.borrow_mut().take();
            if preedit {
                let marked = (range.0, range.0 + text.encode_utf16().count() as u32);
                let selected = selection
                    .map(|r| (marked.0 + r.0, marked.0 + r.1))
                    .unwrap_or(marked);
                // The event can navigate or focus another element. Do not mark that new element.
                let after = snapshot(window)?.ok_or(NativeTextError::StaleContext)?;
                if after.document != expected.document
                    || after.element != expected.element
                    || after.focus_sequence != expected.focus_sequence
                {
                    return Err(NativeTextError::StaleContext);
                }
                if after.text != expected_text
                    || !valid_range(&after.text, marked)
                    || !valid_range(&after.text, selected)
                {
                    return Err(NativeTextError::EditRefused);
                }
                select(window, selected)?;
                let mut after = snapshot(window)?.ok_or(NativeTextError::StaleContext)?;
                after.marked = Some(marked);
                *window.native_text().marked.borrow_mut() = Some(after);
            }
        },
    }
    snapshot(window)
}
#[cfg(test)]
mod tests {
    use super::valid_range;
    #[test]
    fn ranges_are_utf16_and_cannot_split_surrogates() {
        assert!(valid_range("a😀z", (1, 3)));
        assert!(!valid_range("a😀z", (1, 2)));
        assert!(!valid_range("abc", (3, 2)));
        assert!(!valid_range("abc", (0, 4)));
        assert!(valid_range("", (0, 0)));
    }
}
