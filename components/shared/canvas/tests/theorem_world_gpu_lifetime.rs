/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Actual WebGL message/callback ownership, with a drop-observing producer stand-in.
//! No graphics command, document admission or native product proof is claimed.
use servo_base::generic_channel::GenericCallback;
use servo_base::id::{BrowsingContextId, Index, PipelineId, PipelineNamespaceId, WebViewId};
use servo_base::theorem_world_gpu::*;
use servo_canvas_traits::webgl::{WebGLChan, WebGLMsg, webgl_channel};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

struct ProducerLease(Arc<AtomicBool>);
impl Drop for ProducerLease {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}
fn request(
    released: &Arc<AtomicBool>,
) -> (
    WebGLMsg,
    GenericCallback<Result<TheoremWorldTextureReceipt, TheoremWorldTextureError>>,
) {
    let source = TheoremWorldSource {
        registry: 1,
        surface: 2,
        resource_generation: 3,
        width: 4,
        height: 4,
    };
    let binding = TheoremWorldTextureBinding {
        webview: WebViewId::mock_for_testing(BrowsingContextId {
            namespace_id: PipelineNamespaceId(1),
            index: Index::new(1).unwrap(),
        }),
        document: PipelineId {
            namespace_id: PipelineNamespaceId(1),
            index: Index::new(2).unwrap(),
        },
        presentation_epoch: 1,
        challenge: "a".repeat(32),
        context: 1,
        texture: 2,
        source,
    };
    let frame = TheoremWorldFrame {
        surface_address: 1,
        source,
        frame_generation: 1,
        transport_resource_generation: 1,
        bytes_per_row: 16,
        pixel_format: u32::from_be_bytes(*b"BGRA"),
        premultiplied_alpha: true,
    };
    let lease = ProducerLease(released.clone());
    let callback = GenericCallback::new(move |_| {
        let _ = &lease;
    })
    .unwrap();
    let (result, _receiver) = webgl_channel().unwrap();
    (
        WebGLMsg::TheoremWorldImport(binding, frame, result, callback.clone()),
        callback,
    )
}

#[test]
fn queued_gpu_message_keeps_lease_after_script_callback_disappears() {
    let released = Arc::new(AtomicBool::new(false));
    let (message, script_callback) = request(&released);
    let (sender, receiver) = webgl_channel::<WebGLMsg>().unwrap();
    WebGLChan(sender).send(message).unwrap();
    drop(script_callback);
    assert!(
        !released.load(Ordering::SeqCst),
        "queued GPU message owns its completion guard"
    );
    let (entered, entered_receiver) = std::sync::mpsc::channel();
    let (finish, finish_receiver) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let WebGLMsg::TheoremWorldImport(_, _, _, completion_guard) = receiver.recv().unwrap()
        else {
            panic!("wrong worker message");
        };
        entered.send(()).unwrap();
        finish_receiver
            .recv_timeout(Duration::from_secs(5))
            .unwrap();
        // This controlled point stands for the owner's completed GPU read.
        drop(completion_guard);
    });
    entered_receiver
        .recv_timeout(Duration::from_secs(5))
        .unwrap();
    assert!(
        !released.load(Ordering::SeqCst),
        "worker still holds native producer lifetime"
    );
    finish.send(()).unwrap();
    worker.join().unwrap();
    assert!(
        released.load(Ordering::SeqCst),
        "completed worker releases last guard"
    );
}

#[test]
fn unsent_gpu_message_releases_lease_when_queue_is_closed() {
    let released = Arc::new(AtomicBool::new(false));
    let (message, script_callback) = request(&released);
    let (sender, receiver) = webgl_channel::<WebGLMsg>().unwrap();
    drop(receiver);
    drop(script_callback);
    assert!(WebGLChan(sender).send(message).is_err());
    assert!(
        released.load(Ordering::SeqCst),
        "no queued GPU read can retain the lease"
    );
}
