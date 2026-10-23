/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! DOM positions around inline formatting, backed by Servo's Unicode-aware Rope.
use crate::dom::abstractrange::bp_position;
use crate::dom::characterdata::CharacterData;
use crate::dom::iterators::ShadowIncluding;
use crate::dom::node::Node;
use crate::dom::types::{Document, HTMLBRElement, Selection, Text};
use embedder_traits::{EditingDirection, EditingMotion, ModifySelection};
use js::context::{JSContext, NoGC};
use script_bindings::codegen::GenericBindings::SelectionBinding::SelectionMethods;
use script_bindings::inheritance::Castable;
use script_bindings::root::DomRoot;
use servo_base::text::{
    AssumeUnder4GB, Utf16CodeUnits, Utf32CodeUnits, Utf32CodeUnitsOrNodeOffset,
};
use servo_base::{Rope, RopeMovement};

struct TextPositions {
    rope: Rope,
    spans: Vec<(DomRoot<Node>, u32, u32)>,
}
impl TextPositions {
    fn new(root: &Node, no_gc: &NoGC) -> Self {
        fn append(
            node: &Node,
            root: &Node,
            text: &mut String,
            spans: &mut Vec<(DomRoot<Node>, u32, u32)>,
            no_gc: &NoGC,
        ) {
            if !node.is_visible(no_gc)
                || (root.is_editing_host() && node.editing_host_of().as_deref() != Some(root))
            {
                return;
            }
            let block = node != root && node.is_block_node();
            if block && !text.is_empty() && !text.ends_with('\n') {
                text.push('\n');
            }
            if let Some(data) = node.downcast::<Text>() {
                let start = Utf16CodeUnits::length_of(AssumeUnder4GB, text).0;
                text.push_str(&data.upcast::<CharacterData>().data());
                let end = Utf16CodeUnits::length_of(AssumeUnder4GB, text).0;
                spans.push((DomRoot::from_ref(node), start, end));
            } else if node.is::<HTMLBRElement>() {
                text.push('\n');
            } else {
                for child in node.children() {
                    append(&child, root, text, spans, no_gc);
                }
            }
            if block && !text.ends_with('\n') {
                text.push('\n');
            }
        }
        let mut text = String::new();
        let mut spans = Vec::new();
        append(root, root, &mut text, &mut spans, no_gc);
        Self {
            rope: Rope::new(text),
            spans,
        }
    }
    fn offset(&self, node: &Node, offset: u32, no_gc: &NoGC) -> u32 {
        for (text, start, end) in &self.spans {
            if &**text == node {
                return start + offset.min(end - start);
            }
            if bp_position(no_gc, node, offset, text, 0) != std::cmp::Ordering::Greater {
                return *start;
            }
        }
        self.spans.last().map_or(0, |s| s.2)
    }
    fn position(&self, offset: u32) -> Option<(DomRoot<Node>, u32)> {
        for (node, start, end) in &self.spans {
            if offset <= *end {
                return Some((node.clone(), offset.saturating_sub(*start)));
            }
        }
        self.spans
            .last()
            .map(|(node, start, end)| (node.clone(), end - start))
    }
}

impl Document {
    pub(crate) fn move_contenteditable_cursor(
        &self,
        cx: &mut JSContext,
        direction: EditingDirection,
        motion: EditingMotion,
        modify: ModifySelection,
    ) -> bool {
        let Some(selection) = self.selection() else {
            return false;
        };
        let Some(focus) = selection.GetFocusNode(cx) else {
            return false;
        };
        let Some(host) = focus.editing_host_of() else {
            return false;
        };
        let forward = matches!(direction, EditingDirection::Forward);
        if matches!(modify, ModifySelection::No)
            && !selection.collapsed()
            && matches!(motion, EditingMotion::Grapheme | EditingMotion::Character)
        {
            let Some(range) = selection.active_range(cx) else {
                return false;
            };
            let (node, offset) = if forward {
                (range.end_container(), range.end_offset())
            } else {
                (range.start_container(), range.start_offset())
            };
            return selection.Collapse(cx, Some(&node), offset).is_ok();
        }
        let positions = TextPositions::new(&host, cx.no_gc());
        let offset = positions.offset(&focus, selection.FocusOffset(cx), cx.no_gc());
        let rope = &positions.rope;
        let index = rope
            .utf8_offset_to_rope_index(rope.utf16_offset_to_utf8_offset(Utf16CodeUnits(offset)));
        if matches!(
            motion,
            EditingMotion::Line | EditingMotion::Page | EditingMotion::LineStartOrEnd
        ) {
            // Hit-test the rendered adjacent line at the caret's inline position. This
            // uses layout line fragments, so soft wraps are not treated as one line.
            let focus_offset = selection.FocusOffset(cx);
            if let Some(text) = focus.downcast::<Text>() {
                let index = Utf16CodeUnits(focus_offset)
                    .to_utf32_code_units_in(&text.upcast::<CharacterData>().data());
                let caret = self
                    .window()
                    .text_range_rects_query(&[(focus.to_trusted_node_address(), index..index)]);
                let remembered = selection.caret_navigation_position();
                let caret = caret[0].iter().min_by_key(|rect| {
                    remembered.map_or(app_units::Au(0), |(_, y)|
                        ((rect.min_y() + rect.max_y()) / 2 - app_units::Au::from_f32_px(y)).abs())
                });
                if let Some(caret) = caret {
                    let requests: Vec<_> = positions
                        .spans
                        .iter()
                        .map(|(node, _, _)| {
                            let data = node
                                .downcast::<Text>()
                                .unwrap()
                                .upcast::<CharacterData>()
                                .data();
                            (
                                node.to_trusted_node_address(),
                                Utf32CodeUnits(0)
                                    ..Utf16CodeUnits(node.len()).to_utf32_code_units_in(&data),
                            )
                        })
                        .collect();
                    let rects = self.window().text_range_rects_query(&requests);
                    let y = caret.origin.y + caret.size.height / 2;
                    let line = if matches!(motion, EditingMotion::LineStartOrEnd) {
                        rects
                            .iter()
                            .flatten()
                            .filter(|r| r.min_y() <= y && r.max_y() >= y)
                            .copied()
                            .reduce(|a, b| a.union(&b))
                    } else {
                        rects
                            .iter()
                            .flatten()
                            .filter(|r| {
                                if forward {
                                    (r.origin.y + r.size.height / 2) > caret.max_y()
                                } else {
                                    (r.origin.y + r.size.height / 2) < caret.min_y()
                                }
                            })
                            .min_by_key(|r| ((r.origin.y + r.size.height / 2) - y).abs())
                            .copied()
                    };
                    if let Some(line) = line {
                        let x = if matches!(motion, EditingMotion::LineStartOrEnd) {
                            if forward { line.max_x() } else { line.min_x() }
                        } else {
                            remembered.and_then(|(x, _)| x).map_or(caret.origin.x, app_units::Au::from_f32_px)
                        };
                        if let Some((node, offset)) =
                            self.window().text_index_query_on_node_for_event(
                                &host,
                                euclid::Point2D::new(x, line.origin.y + line.size.height / 2),
                            )
                        {
                            if matches!(modify, ModifySelection::Yes) {
                                selection.collapse_or_extend_to_dom_position(
                                    cx,
                                    &node,
                                    Utf32CodeUnitsOrNodeOffset(offset.0),
                                );
                            } else {
                                selection.collapse_to_dom_position(
                                    cx,
                                    &node,
                                    Utf32CodeUnitsOrNodeOffset(offset.0),
                                );
                            }
                            selection.remember_caret_navigation_position(
                                Some(x.to_f32_px()), (line.origin.y + line.size.height / 2).to_f32_px());
                            return true;
                        }
                    }
                }
            }
        }
        let movement = match motion {
            EditingMotion::Character => RopeMovement::Character,
            EditingMotion::Grapheme => RopeMovement::Grapheme,
            EditingMotion::Word => RopeMovement::Word,
            EditingMotion::Line | EditingMotion::Page => RopeMovement::Line,
            EditingMotion::LineStartOrEnd => RopeMovement::LineStartOrEnd,
            EditingMotion::DocumentStartOrEnd => RopeMovement::RopeStartOrEnd,
        };
        let next = rope.move_by(index, movement, if forward { 1 } else { -1 });
        let Some((node, offset)) = positions.position(rope.index_to_utf16_offset(next).0) else {
            return false;
        };
        if matches!(modify, ModifySelection::Yes) {
            selection.Extend(cx, &node, offset).is_ok()
        } else {
            selection.Collapse(cx, Some(&node), offset).is_ok()
        }
    }
}

/// Word selection shares the same Unicode segmentation as text controls, but
/// spans text nodes so an inline formatting boundary does not split a word.
pub(crate) fn select_word(
    cx: &mut JSContext,
    selection: &Selection,
    node: &Node,
    offset: Utf32CodeUnitsOrNodeOffset,
    contain: Option<&Node>,
) -> bool {
    let root = node
        .inclusive_ancestors(ShadowIncluding::No)
        .find(|ancestor| {
            Some(&**ancestor) == contain || ancestor.is_block_node() || ancestor.is_editing_host()
        })
        .unwrap_or_else(|| DomRoot::from_ref(node));
    let positions = TextPositions::new(&root, cx.no_gc());
    let utf16 = if let Some(text) = node.downcast::<Text>() {
        Utf32CodeUnits(offset.0)
            .to_utf16_code_units_in(AssumeUnder4GB, &text.upcast::<CharacterData>().data())
            .0
    } else {
        offset.0
    };
    let offset = positions.offset(node, utf16, cx.no_gc());
    let rope = &positions.rope;
    let index =
        rope.utf8_offset_to_rope_index(rope.utf16_offset_to_utf8_offset(Utf16CodeUnits(offset)));
    let word = rope.relevant_word_boundaries(index);
    let Some((start, start_offset)) = positions.position(rope.index_to_utf16_offset(word.start).0)
    else {
        return false;
    };
    let Some((end, end_offset)) = positions.position(rope.index_to_utf16_offset(word.end).0) else {
        return false;
    };
    selection
        .SetBaseAndExtent(cx, &start, start_offset, &end, end_offset)
        .is_ok()
}
