/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Rounded text selection highlights.
//!
//! The selected text of each line box is painted as one shape. Its outer corners are
//! rounded; corners where the selection continues straight onto the previous or next line
//! stay square; and where a neighbouring line's selection reaches further out, the joint
//! gets a concave fillet, the way code editors draw their selections.

use std::ops::Range;
use std::rc::Rc;

use app_units::{AU_PER_PX, Au};
use euclid::{Point2D, Size2D};
use style::Zero;
use style::properties::ComputedValues;
use webrender_api::units::{LayoutPoint, LayoutRect, LayoutSize, LayoutVector2D};
use webrender_api::{BorderRadius, ClipChainId, ClipMode, ColorF, ComplexClipRegion};

use super::paint_traversal::TraversalState;
use super::{DisplayListBuilder, ToWebRender};
use crate::fragment_tree::{Fragment, TextFragment};
use crate::geom::{PhysicalPoint, PhysicalRect};

/// The radius of a selection's rounded corners, unless the shape is too small for it.
const CORNER_RADIUS: Au = Au(4 * AU_PER_PX);

/// Edges closer than this count as aligned.
const ALIGNED: Au = Au(AU_PER_PX / 2);

/// The part of a [`TextFragment`] that is selected, as advances from the fragment's start.
pub(super) struct TextSelectionExtent {
    pub start: Au,
    pub end: Au,
    /// Whether the selection is empty, so that this is a caret position.
    pub is_collapsed: bool,
}

pub(super) fn text_selection_extent(fragment: &TextFragment) -> Option<TextSelectionExtent> {
    let run_data = &fragment.run_data;
    let selection = (*run_data.selection.borrow())?;

    // The selection character range is in pre-transformed character offsets, so use the
    // OffsetMap contained within `run_data` to convert it to post-transformed character
    // offsets. This allows updating this selection directly from the DOM (skipping layout).
    let selection_character_range = run_data.map_dom_range_to_transformed_range(selection);

    if fragment.character_range_in_dom_node.start > selection_character_range.end ||
        fragment.character_range_in_dom_node.end < selection_character_range.start
    {
        return None;
    }

    // When there is an active selection, the line is empty, and there is a forced linebreak,
    // layout will push an empty fragment in order to trigger painting of the cursor on an empty line.
    // This code ensure that it is only painted if the cursor is on the starting index of the empty
    // fragment.
    if fragment.is_empty_for_text_cursor &&
        !fragment
            .character_range_in_dom_node
            .contains(&selection_character_range.start)
    {
        return None;
    }

    let mut current_character_index = fragment.character_range_in_dom_node.start;
    let mut current_advance = Au::zero();
    let mut start_advance = None;
    let mut end_advance = None;
    for glyph_store in fragment.glyphs.iter() {
        let glyph_store_character_count = glyph_store.character_count();
        if current_character_index + glyph_store_character_count < selection_character_range.start
        {
            current_advance += glyph_store.total_advance() +
                (fragment.justification_adjustment * glyph_store.total_word_separators() as i32);
            current_character_index += glyph_store_character_count;
            continue;
        }

        if current_character_index >= selection_character_range.end {
            break;
        }

        for glyph in glyph_store.glyphs() {
            if current_character_index >= selection_character_range.start {
                start_advance = start_advance.or(Some(current_advance));
            }

            current_character_index += glyph.character_count();
            current_advance += glyph.advance();
            if glyph.char_is_word_separator() {
                current_advance += fragment.justification_adjustment;
            }

            if current_character_index <= selection_character_range.end {
                end_advance = Some(current_advance);
            }
        }
    }

    Some(TextSelectionExtent {
        start: start_advance.unwrap_or(current_advance),
        end: end_advance.unwrap_or(current_advance),
        is_collapsed: selection_character_range.is_empty(),
    })
}

/// How one corner of a line's selection meets the rest of the selection.
#[derive(Clone, Copy, Debug, Default)]
struct Corner {
    /// The radius of the rounded outer corner, or zero.
    rounded: Au,
    /// The radius of the concave fillet outside the corner, which joins a neighbouring
    /// line whose selection reaches further out, or zero.
    fillet: Au,
}

impl Corner {
    /// `outset` is how much further out than this edge the neighbouring line's selection
    /// reaches, or `None` when there is no neighbouring selection to join.
    fn new(outset: Option<Au>) -> Self {
        match outset {
            None => Self {
                rounded: CORNER_RADIUS,
                fillet: Au::zero(),
            },
            Some(outset) if outset.abs() < ALIGNED => Self::default(),
            // The two lines share the distance between their edges, half each.
            Some(outset) if outset > Au::zero() => Self {
                rounded: Au::zero(),
                fillet: CORNER_RADIUS.min(outset / 2),
            },
            Some(outset) => Self {
                rounded: CORNER_RADIUS.min(-outset / 2),
                fillet: Au::zero(),
            },
        }
    }
}

/// The selected part of one line box.
#[derive(Debug)]
pub(crate) struct LineSelection {
    /// From the start of the line's first selected glyph to the end of its last, the full
    /// height of the line box.
    span: PhysicalRect<Au>,
    top_left: Corner,
    top_right: Corner,
    bottom_right: Corner,
    bottom_left: Corner,
}

impl LineSelection {
    /// The selection of each of a block container's `children` that is a line box with
    /// selected text, by child index. Empty when none of them has any.
    pub(crate) fn for_line_boxes(
        origin: PhysicalPoint<Au>,
        children: &[Fragment],
    ) -> Vec<Option<Rc<Self>>> {
        let spans: Vec<(usize, Option<PhysicalRect<Au>>)> = children
            .iter()
            .enumerate()
            .filter_map(|(index, child)| match child {
                Fragment::Positioning(line) if line.is_line_box() => {
                    let rect = line.base.rect().translate(origin.to_vector());
                    let span = selected_span(rect.origin, &line.children).map(|span| {
                        PhysicalRect::new(
                            Point2D::new(span.start, rect.min_y()),
                            Size2D::new(span.end - span.start, rect.height()),
                        )
                    });
                    Some((index, span))
                },
                _ => None,
            })
            .collect();
        if spans.iter().all(|(_, span)| span.is_none()) {
            return Vec::new();
        }

        let mut selections = vec![None; children.len()];
        for (position, &(index, span)) in spans.iter().enumerate() {
            let Some(span) = span else {
                continue;
            };
            let previous = position
                .checked_sub(1)
                .and_then(|previous| spans[previous].1)
                .filter(|previous| (previous.max_y() - span.min_y()).abs() < ALIGNED);
            let next = spans
                .get(position + 1)
                .and_then(|next| next.1)
                .filter(|next| (next.min_y() - span.max_y()).abs() < ALIGNED);
            selections[index] = Some(Rc::new(Self::new(span, previous, next)));
        }
        selections
    }

    fn new(
        span: PhysicalRect<Au>,
        previous: Option<PhysicalRect<Au>>,
        next: Option<PhysicalRect<Au>>,
    ) -> Self {
        // Lines join only where their selections overlap horizontally.
        let overlapping = |neighbor: Option<PhysicalRect<Au>>| {
            neighbor.filter(|neighbor| {
                neighbor.min_x() < span.max_x() && neighbor.max_x() > span.min_x()
            })
        };
        let left = |neighbor: Option<PhysicalRect<Au>>| {
            Corner::new(overlapping(neighbor).map(|neighbor| span.min_x() - neighbor.min_x()))
        };
        let right = |neighbor: Option<PhysicalRect<Au>>| {
            Corner::new(overlapping(neighbor).map(|neighbor| neighbor.max_x() - span.max_x()))
        };

        let mut selection = Self {
            span,
            top_left: left(previous),
            top_right: right(previous),
            bottom_right: right(next),
            bottom_left: left(next),
        };
        let max_rounded = span.width().min(span.height()) / 2;
        let max_fillet = span.height() / 2;
        for corner in [
            &mut selection.top_left,
            &mut selection.top_right,
            &mut selection.bottom_right,
            &mut selection.bottom_left,
        ] {
            corner.rounded = corner.rounded.min(max_rounded);
            corner.fillet = corner.fillet.min(max_fillet);
        }
        selection
    }

    /// Paint `rect`, one text fragment's part of this line's selection, clipped to the
    /// line's shape.
    pub(super) fn paint(
        &self,
        builder: &mut DisplayListBuilder,
        state: &TraversalState,
        rect: LayoutRect,
        color: ColorF,
        style: &ComputedValues,
    ) {
        let span = self.span.to_webrender();
        let aligned = ALIGNED.to_f32_px();
        // Only the fragments at either end of the line reach its corners.
        let at_start = rect.min.x - span.min.x < aligned;
        let at_end = span.max.x - rect.max.x < aligned;

        let rounded = |corner: Corner, applies: bool| {
            let radius = if applies {
                corner.rounded.to_f32_px()
            } else {
                0.0
            };
            LayoutSize::new(radius, radius)
        };
        let radii = BorderRadius {
            top_left: rounded(self.top_left, at_start),
            top_right: rounded(self.top_right, at_end),
            bottom_right: rounded(self.bottom_right, at_end),
            bottom_left: rounded(self.bottom_left, at_start),
        };
        let mut common = builder.common_properties(state, rect, style);
        if let Some(clip_chain_id) = builder.maybe_create_clip(state, radii, span, false) {
            common.clip_chain_id = clip_chain_id;
        }
        builder.wr().push_rect(&common, rect, color);

        // Each fillet is the square outside its corner, less the circle that curves the
        // edge into the neighbouring line. The signs point away from the line horizontally
        // and into it vertically.
        for (corner, applies, point, outward, inward) in [
            (self.top_left, at_start, span.min, -1.0, 1.0),
            (
                self.top_right,
                at_end,
                LayoutPoint::new(span.max.x, span.min.y),
                1.0,
                1.0,
            ),
            (self.bottom_right, at_end, span.max, 1.0, -1.0),
            (
                self.bottom_left,
                at_start,
                LayoutPoint::new(span.min.x, span.max.y),
                -1.0,
                -1.0,
            ),
        ] {
            let fillet = corner.fillet.to_f32_px();
            if !applies || fillet <= 0.0 {
                continue;
            }

            let center = point + LayoutVector2D::new(outward * fillet, inward * fillet);
            let square = LayoutRect::from_points([point, center]);
            let circle = LayoutRect::new(
                center - LayoutVector2D::splat(fillet),
                center + LayoutVector2D::splat(fillet),
            );

            let spatial_id = builder.spatial_id(state.spatial_id);
            let circle_clip = builder.wr().define_clip_rounded_rect(
                spatial_id,
                ComplexClipRegion {
                    rect: circle,
                    radii: BorderRadius::uniform(fillet),
                    mode: ClipMode::ClipOut,
                },
            );
            let parent_clip_chain_id = match builder.clip_chain_id(state.clip_id) {
                ClipChainId::INVALID => None,
                parent => Some(parent),
            };
            let mut common = builder.common_properties(state, square, style);
            common.clip_chain_id = builder
                .wr()
                .define_clip_chain(parent_clip_chain_id, [circle_clip]);
            builder.wr().push_rect(&common, square, color);
        }
    }
}

/// The horizontal extent of the selected text among a line box's `fragments`, whose
/// containing block starts at `origin`.
fn selected_span(origin: PhysicalPoint<Au>, fragments: &[Fragment]) -> Option<Range<Au>> {
    fragments
        .iter()
        .filter_map(|fragment| match fragment {
            Fragment::Text(text) => {
                let extent = text_selection_extent(text)
                    .filter(|extent| !extent.is_collapsed && extent.end > extent.start)?;
                let x = origin.x + text.base.rect().origin.x;
                Some(x + extent.start..x + extent.end)
            },
            Fragment::Box(inline_box) if inline_box.with_style().is_inline_box() => selected_span(
                origin + inline_box.content_rect().origin.to_vector(),
                &inline_box.children,
            ),
            Fragment::LayoutRoot(layout_root) => {
                selected_span(origin, std::slice::from_ref(&*layout_root.inner()))
            },
            _ => None,
        })
        .reduce(|first, second| first.start.min(second.start)..first.end.max(second.end))
}
