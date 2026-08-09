/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::sync::Arc;

use app_units::Au;
use euclid::{Point2D, Rect, Size2D};
use layout_api::{
    BoxAreaType, DocumentLayoutSnapshotComputedStyle, DocumentLayoutSnapshotProjection,
    DocumentLayoutSnapshotProjectionNode,
};
use rustc_hash::FxHashMap;
use style::dom::OpaqueNode;
use style::properties::{ComputedValues, LonghandId, PropertyDeclarationId};
use style_traits::CSSPixel;

use super::paint_traversal::{PaintTraversal, PaintTraversalHandler};
use super::{StackingContext, StackingContextTree, TraversalState};
use crate::fragment_tree::{
    BoxFragment, BoxFragmentWithStyle, ContainingBlockCalculation, Fragment, FragmentTree,
    IFrameFragment, ImageFragment, PositioningFragment, TextFragment,
};
use crate::geom::PhysicalRect;

pub(crate) fn build_document_layout_snapshot_projection(
    fragment_tree: &FragmentTree,
    stacking_context_tree: &StackingContextTree,
) -> DocumentLayoutSnapshotProjection {
    let mut nodes = Vec::new();
    let mut node_indices = FxHashMap::default();

    let _ = fragment_tree.find(|fragment, _, _| {
        collect_fragment(fragment, &mut nodes, &mut node_indices);
        None::<()>
    });

    let mut paint_join = PaintJoin {
        nodes: &mut nodes,
        node_indices: &node_indices,
        paint_order: 0,
        next_stacking_context: 0,
        current_stacking_context: 0,
    };
    PaintTraversal::traverse(
        &stacking_context_tree.root_stacking_context,
        &mut paint_join,
    );

    DocumentLayoutSnapshotProjection { nodes }
}

fn collect_fragment(
    fragment: &Fragment,
    nodes: &mut Vec<DocumentLayoutSnapshotProjectionNode>,
    node_indices: &mut FxHashMap<OpaqueNode, usize>,
) {
    let Some(tag) = fragment.tag() else {
        return;
    };
    if !tag.pseudo_element_chain.is_empty() {
        return;
    }

    let index = *node_indices.entry(tag.node).or_insert_with(|| {
        let computed = fragment
            .base()
            .map(|base| computed_style(&base.style()))
            .unwrap_or_default();
        let index = nodes.len();
        nodes.push(DocumentLayoutSnapshotProjectionNode {
            node: tag.node.into(),
            bbox: None,
            client_rect: None,
            scroll_rect: None,
            paint_order: None,
            stacking_context: None,
            computed,
            visible: false,
            scrollable: false,
        });
        index
    });

    let node = &mut nodes[index];
    let bbox = fragment
        .cumulative_box_area_rect(
            BoxAreaType::Border,
            ContainingBlockCalculation::AlreadyDoneWithStackingContextTree,
        )
        .map(au_rect_to_i32);
    union_optional_rect(&mut node.bbox, bbox);

    let client_rect = fragment
        .cumulative_box_area_rect(
            BoxAreaType::Padding,
            ContainingBlockCalculation::AlreadyDoneWithStackingContextTree,
        )
        .map(au_rect_to_i32);
    union_optional_rect(&mut node.client_rect, client_rect);

    if let Some(box_fragment) = fragment.retrieve_box_fragment() {
        let scroll_rect = box_fragment.offset_by_containing_block(
            &box_fragment.with_style().scrollable_overflow(),
            ContainingBlockCalculation::AlreadyDoneWithStackingContextTree,
        );
        union_optional_rect(&mut node.scroll_rect, Some(au_rect_to_i32(scroll_rect)));
    }

    node.visible = node.bbox.is_some_and(|rect| {
        rect.size.width > 0
            && rect.size.height > 0
            && node.computed.visibility.as_deref() != Some("hidden")
            && node.computed.visibility.as_deref() != Some("collapse")
            && node.computed.opacity.as_deref() != Some("0")
    });
    node.scrollable = match (node.client_rect, node.scroll_rect) {
        (Some(client), Some(scroll)) => {
            scroll.size.width > client.size.width || scroll.size.height > client.size.height
        },
        _ => false,
    };
    if !node.scrollable {
        node.scroll_rect = None;
    }
}

fn computed_style(style: &ComputedValues) -> DocumentLayoutSnapshotComputedStyle {
    let value =
        |longhand| Some(style.computed_value_to_string(PropertyDeclarationId::Longhand(longhand)));
    DocumentLayoutSnapshotComputedStyle {
        display: value(LonghandId::Display),
        visibility: value(LonghandId::Visibility),
        position: value(LonghandId::Position),
        overflow_x: value(LonghandId::OverflowX),
        overflow_y: value(LonghandId::OverflowY),
        opacity: value(LonghandId::Opacity),
        pointer_events: value(LonghandId::PointerEvents),
        cursor: value(LonghandId::Cursor),
        white_space: value(LonghandId::WhiteSpaceCollapse),
        font_size: value(LonghandId::FontSize),
    }
}

fn au_rect_to_i32(rect: PhysicalRect<Au>) -> Rect<i32, CSSPixel> {
    Rect::new(
        Point2D::new(rect.origin.x.to_f32_px(), rect.origin.y.to_f32_px()),
        Size2D::new(rect.size.width.to_f32_px(), rect.size.height.to_f32_px()),
    )
    .round()
    .to_i32()
}

fn union_optional_rect(
    destination: &mut Option<Rect<i32, CSSPixel>>,
    incoming: Option<Rect<i32, CSSPixel>>,
) {
    let Some(incoming) = incoming else {
        return;
    };
    *destination = Some(destination.map_or(incoming, |current| current.union(&incoming)));
}

struct PaintJoin<'a> {
    nodes: &'a mut [DocumentLayoutSnapshotProjectionNode],
    node_indices: &'a FxHashMap<OpaqueNode, usize>,
    paint_order: u32,
    next_stacking_context: u32,
    current_stacking_context: u32,
}

impl PaintJoin<'_> {
    fn visit_tag(&mut self, tag: Option<crate::fragment_tree::Tag>) {
        let paint_order = self.paint_order;
        self.paint_order = self.paint_order.saturating_add(1);
        let Some(tag) = tag else {
            return;
        };
        if !tag.pseudo_element_chain.is_empty() {
            return;
        }
        let Some(index) = self.node_indices.get(&tag.node) else {
            return;
        };
        let node = &mut self.nodes[*index];
        node.paint_order = Some(paint_order);
        node.stacking_context = Some(self.current_stacking_context);
    }
}

impl PaintTraversalHandler for PaintJoin<'_> {
    type StackingContextState = u32;

    fn visit_stacking_context(
        &mut self,
        _stacking_context: &StackingContext,
    ) -> Self::StackingContextState {
        let previous = self.current_stacking_context;
        self.current_stacking_context = self.next_stacking_context;
        self.next_stacking_context = self.next_stacking_context.saturating_add(1);
        previous
    }

    fn leave_stacking_context(
        &mut self,
        _state: &TraversalState,
        previous: Self::StackingContextState,
    ) {
        self.current_stacking_context = previous;
    }

    fn visit_box(&mut self, _state: &TraversalState, fragment: &BoxFragmentWithStyle<'_>) {
        self.visit_tag(fragment.box_fragment.base.tag);
    }

    fn visit_iframe(&mut self, _state: &TraversalState, fragment: &Arc<IFrameFragment>) {
        self.visit_tag(fragment.base.tag);
    }

    fn visit_image(
        &mut self,
        _state: &TraversalState,
        _containing_block: PhysicalRect<Au>,
        fragment: &Arc<ImageFragment>,
    ) {
        self.visit_tag(fragment.base.tag);
    }

    fn visit_text(
        &mut self,
        _state: &TraversalState,
        _containing_block: PhysicalRect<Au>,
        fragment: &Arc<TextFragment>,
    ) {
        self.visit_tag(fragment.base.tag);
    }

    fn visit_positioning(&mut self, _state: &TraversalState, fragment: &Arc<PositioningFragment>) {
        self.visit_tag(fragment.base.tag);
    }

    fn visit_box_for_root_background(&mut self, _state: &TraversalState) {
        self.visit_tag(None);
    }

    fn visit_box_for_outline(&mut self, _state: &TraversalState, fragment: &Arc<BoxFragment>) {
        self.visit_tag(fragment.base.tag);
    }

    fn visit_box_for_collapsed_table_borders(
        &mut self,
        _state: &TraversalState,
        fragment: &BoxFragmentWithStyle<'_>,
    ) {
        self.visit_tag(fragment.box_fragment.base.tag);
    }
}
