/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use servo_base::id::{BrowsingContextId, Index, PipelineId, PipelineNamespaceId, WebViewId};
use servo_base::theorem_world_gpu::*;

fn binding() -> TheoremWorldTextureBinding {
    TheoremWorldTextureBinding {
        webview: WebViewId::mock_for_testing(BrowsingContextId {
            namespace_id: PipelineNamespaceId(2),
            index: Index::new(1).unwrap(),
        }),
        document: PipelineId {
            namespace_id: PipelineNamespaceId(2),
            index: Index::new(2).unwrap(),
        },
        presentation_epoch: u64::MAX,
        challenge: "a".repeat(32),
        context: 3,
        texture: 4,
        source: TheoremWorldSource {
            registry: 5,
            surface: 6,
            resource_generation: 7,
            width: 640,
            height: 400,
        },
    }
}
fn frame(binding: &TheoremWorldTextureBinding) -> TheoremWorldFrame {
    TheoremWorldFrame {
        surface_address: 1,
        source: binding.source,
        frame_generation: 9,
        transport_resource_generation: 10,
        bytes_per_row: 2560,
        pixel_format: u32::from_be_bytes(*b"BGRA"),
        premultiplied_alpha: true,
    }
}
#[test]
fn source_generation_and_monotonic_frame_are_independent_admission_requirements() {
    let binding = binding();
    let admitted = frame(&binding);
    assert_eq!(validate_frame(&binding, &admitted, 8), Ok(()));
    assert_eq!(
        validate_frame(&binding, &admitted, 9),
        Err(TheoremWorldTextureError::InvalidFrame)
    );
    for mutate in [
        |f: &mut TheoremWorldFrame| f.source.registry += 1,
        |f: &mut TheoremWorldFrame| f.source.surface += 1,
        |f: &mut TheoremWorldFrame| f.source.resource_generation += 1,
        |f: &mut TheoremWorldFrame| f.source.width += 1,
        |f: &mut TheoremWorldFrame| f.source.height += 1,
        |f: &mut TheoremWorldFrame| f.surface_address = 0,
        |f: &mut TheoremWorldFrame| f.bytes_per_row = 2559,
        |f: &mut TheoremWorldFrame| f.premultiplied_alpha = false,
        |f: &mut TheoremWorldFrame| f.pixel_format = u32::from_be_bytes(*b"RGBA"),
        |f: &mut TheoremWorldFrame| f.transport_resource_generation = 0,
    ] {
        let mut wrong = admitted.clone();
        mutate(&mut wrong);
        assert_eq!(
            validate_frame(&binding, &wrong, 8),
            Err(TheoremWorldTextureError::InvalidFrame)
        );
    }
}
#[test]
fn new_transport_allocation_does_not_forge_source_or_frame_identity() {
    let binding = binding();
    let mut next = frame(&binding);
    next.transport_resource_generation = 11;
    next.frame_generation = 10;
    assert_eq!(validate_frame(&binding, &next, 9), Ok(()));
    next.source.resource_generation += 1;
    assert_eq!(
        validate_frame(&binding, &next, 9),
        Err(TheoremWorldTextureError::InvalidFrame)
    );
}
