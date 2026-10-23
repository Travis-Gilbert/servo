# Stylo 0.22 relative-selector invalidation patch

This is the published `stylo` 0.22.0 crate, retained under its original MPL-2.0 license.
Upstream source commit: `34d25f741765a0bdb894f8271852e633299352d8`.

The relative-selector correction is in `invalidation/element/relative_selector.rs`:
`AncestorEarlierSibling` must break out of the current sibling loop when its
search flags end, then continue with higher ancestors. Returning from the
entire invalidation walk leaves non-subject :has() styles stale after nested
subtrees are appended.

Regression: `css/selectors/invalidation/has-inserted-subtree-non-subject.html`,
plus existing has-in-adjacent-position.html and has-in-sibling-position.html.

A local path dependency is used so downstream embedders use the same corrected
Stylo as the fork without relying on a root-only Cargo patch override. No other
Stylo crate or selector implementation is forked. Remove this vendored crate
when adopting an upstream release containing the correction.

ServoElementSnapshot derives Clone so Servo can invalidate pending old attribute/state
snapshots before detaching a subtree, while retaining them for normal invalidation
after a move. Regression: has-mutation-before-detach.html.
