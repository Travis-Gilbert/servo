/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Connect DOM changes to Stylo's relative-selector invalidation machinery.

#![expect(unsafe_code)]

use script::layout_dom::ServoDangerousStyleElement;
use selectors::Element;
use selectors::matching::ElementSelectorFlags;
use style::dom::TElement;
use style::invalidation::element::element_wrapper::{ElementSnapshot, ElementWrapper};
use style::invalidation::element::invalidator::{InvalidationResult, SiblingTraversalMap};
use style::invalidation::element::relative_selector::{
    DomMutationOperation, RelativeSelectorInvalidator,
};
use style::selector_parser::SnapshotMap;
use style::stylist::Stylist;
use stylo_atoms::Atom;

/// Relative invalidation can travel above the script-selected dirty root. Mark
/// paths to the document root, without adding subtree restyle hints. Traversal
/// still skips clean branches and only rematches the elements selected by Stylo.
pub(crate) fn mark_path_to_root(element: ServoDangerousStyleElement<'_>) {
    let mut ancestor = Some(element);
    while let Some(element) = ancestor {
        unsafe { element.set_dirty_descendants() };
        ancestor = element.traversal_parent();
    }
}

fn invalidated(element: ServoDangerousStyleElement<'_>, result: &InvalidationResult) {
    if result.has_invalidated_self()
        || result.has_invalidated_descendants()
        || result.has_invalidated_siblings()
    {
        mark_path_to_root(element);
    }
}

fn invalidator<'a, 'b, 'dom>(
    element: ServoDangerousStyleElement<'dom>,
    stylist: &'a Stylist,
    snapshots: Option<&'b SnapshotMap>,
    siblings: SiblingTraversalMap<ServoDangerousStyleElement<'dom>>,
) -> RelativeSelectorInvalidator<'a, 'b, ServoDangerousStyleElement<'dom>>
where
    'dom: 'a,
{
    RelativeSelectorInvalidator {
        element,
        quirks_mode: stylist.quirks_mode(),
        snapshot_table: snapshots,
        invalidated,
        sibling_traversal_map: siblings,
        _marker: std::marker::PhantomData,
    }
}

pub(crate) fn invalidate_snapshot(
    element: ServoDangerousStyleElement<'_>,
    stylist: &Stylist,
    snapshots: &SnapshotMap,
) {
    let Some(snapshot) = snapshots.get(&element) else {
        return;
    };
    let states = ElementWrapper::new(element, snapshots).state_changes();
    let old_id = snapshot.has_attrs().then(|| snapshot.id_attr()).flatten();
    let mut old_classes = Vec::new();
    if snapshot.has_attrs() {
        snapshot.each_class(|class| old_classes.push((**class).clone()));
    }
    invalidator(
        element,
        stylist,
        Some(snapshots),
        SiblingTraversalMap::default(),
    )
    .invalidate_relative_selectors_for_this(
        stylist,
        |element, scope, data, quirks, collector| {
            let map = data.relative_selector_invalidation_map();
            if snapshot.id_changed() {
                for id in element.id().into_iter().chain(old_id) {
                    if let Some(deps) = map.id_to_selector.get(id, quirks) {
                        for dep in deps {
                            collector.add_dependency(dep, *element, scope);
                        }
                    }
                }
            }
            if snapshot.class_changed() {
                let mut add_class = |class: &Atom| {
                    if let Some(deps) = map.class_to_selector.get(class, quirks) {
                        for dep in deps {
                            collector.add_dependency(dep, *element, scope);
                        }
                    }
                };
                element.each_class(|class| add_class(class));
                old_classes.iter().for_each(add_class);
            }
            snapshot.each_attr_changed(|attr| {
                if let Some(deps) = map.other_attribute_affecting_selectors.get(attr) {
                    for dep in deps {
                        collector.add_dependency(dep, *element, scope);
                    }
                }
            });
            map.state_affecting_selectors.lookup_with_additional(
                *element,
                quirks,
                old_id,
                &old_classes,
                states,
                |dep| {
                    if dep.state.intersects(states) {
                        collector.add_dependency(&dep.dep, *element, scope);
                    }
                    true
                },
            );
        },
    );
}

/// Called after insertion, but before removal while the old ancestry is intact.
/// The sibling overrides inspect the adjacency without the inserted/removed node.
pub(crate) fn invalidate_mutation(
    element: ServoDangerousStyleElement<'_>,
    stylist: &Stylist,
    inserted: bool,
) -> bool {
    let prev = element.prev_sibling_element();
    let next = element.next_sibling_element();
    let mut inherited = ElementSelectorFlags::empty();
    if let Some(parent) = element.parent_element() {
        inherited |= parent.relative_selector_search_direction()
            & ElementSelectorFlags::RELATIVE_SELECTOR_SEARCH_DIRECTION_ANCESTOR;
    }
    if let Some(prev) = prev {
        inherited |= prev.relative_selector_search_direction();
    }
    if inherited.is_empty() {
        return false;
    }
    if let (Some(prev), Some(next)) = (prev, next) {
        if !inserted
            || prev
                .relative_selector_search_direction()
                .intersects(ElementSelectorFlags::RELATIVE_SELECTOR_SEARCH_DIRECTION_SIBLING)
        {
            element.apply_selector_flags(
                ElementSelectorFlags::RELATIVE_SELECTOR_SEARCH_DIRECTION_SIBLING,
            );
            invalidator(
                prev,
                stylist,
                None,
                SiblingTraversalMap::new(prev, prev.prev_sibling_element(), Some(next)),
            )
            .invalidate_relative_selectors_for_dom_mutation(
                false,
                stylist,
                ElementSelectorFlags::empty(),
                DomMutationOperation::SideEffectPrevSibling,
            );
            invalidator(
                next,
                stylist,
                None,
                SiblingTraversalMap::new(next, Some(prev), next.next_sibling_element()),
            )
            .invalidate_relative_selectors_for_dom_mutation(
                false,
                stylist,
                ElementSelectorFlags::empty(),
                DomMutationOperation::SideEffectNextSibling,
            );
        }
    }
    invalidator(element, stylist, None, SiblingTraversalMap::default())
        .invalidate_relative_selectors_for_dom_mutation(
            true,
            stylist,
            inherited,
            if !inserted {
                DomMutationOperation::Remove
            } else if next.is_none() {
                DomMutationOperation::Append
            } else {
                DomMutationOperation::Insert
            },
        );
    true
}

/// Text mutations change :empty without changing attributes or element state.
pub(crate) fn invalidate_empty(element: ServoDangerousStyleElement<'_>, stylist: &Stylist) -> bool {
    use style::invalidation::element::invalidation_map::TSStateForInvalidation;
    if element.relative_selector_search_direction().is_empty() {
        return false;
    }
    invalidator(element, stylist, None, SiblingTraversalMap::default())
        .invalidate_relative_selectors_for_this(
            stylist,
            |element, scope, data, quirks, collector| {
                data.relative_invalidation_map_attributes()
                    .ts_state_to_selector
                    .lookup_with_additional(
                        *element,
                        quirks,
                        None,
                        &[],
                        stylo_dom::ElementState::empty(),
                        |dependency| {
                            if dependency.state.intersects(TSStateForInvalidation::EMPTY) {
                                collector.add_dependency(&dependency.dep, *element, scope);
                            }
                            true
                        },
                    );
            },
        );
    true
}
