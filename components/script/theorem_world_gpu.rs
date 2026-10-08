/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */
#![expect(unsafe_code)]

//! Fixed native bootstrap. The callback takes typed DOM refs and returns a boolean.
//! Neither the callback nor an engine GPU identifier is installed on the global object.
use crate::dom::bindings::codegen::Bindings::WindowBinding::WindowMethods;
use crate::dom::bindings::conversions::{
    ConversionResult, FromJSValConvertible, SafeToJSValConvertible, get_property_jsval,
    root_from_object_static,
};
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::bindings::root::Dom;
use crate::dom::globalscope::GlobalScope;
use crate::dom::webgl::webgl2renderingcontext::WebGL2RenderingContext;
use crate::dom::webgl::webglobject::WebGLObject;
use crate::dom::webgl::webgltexture::WebGLTexture;
use crate::dom::window::Window;
use js::context::JSContext;
use js::jsapi::{CallArgs, HandleValueArray, JSPROP_PERMANENT, JSPROP_READONLY};
use js::jsapi::{Heap, JSObject};
use js::jsval::{BooleanValue, JSVal, ObjectValue, UndefinedValue};
use js::realm::CurrentRealm;
use js::rust::wrappers2::{Call, JS_ClearPendingException, JS_DefineProperty};
use script_bindings::cell::DomRefCell;
use script_bindings::reflector::DomObject;
use servo_base::generic_channel::GenericCallback;
use servo_base::theorem_world_gpu::*;
use servo_canvas_traits::webgl::{TexFormat, WebGLMsg, webgl_channel};
use std::cell::{Cell, RefCell};

#[derive(JSTraceable, MallocSizeOf, Default)]
pub(crate) struct State {
    textures: DomRefCell<Vec<Destination>>,
    #[no_trace]
    #[ignore_malloc_size_of = "small native registration state"]
    pending: RefCell<Option<Pending>>,
    #[ignore_malloc_size_of = "mozjs traced native callback"]
    callback: Heap<*mut JSObject>,
    attestor_installed: Cell<bool>,
    #[no_trace]
    #[ignore_malloc_size_of = "small native registration outcome"]
    registration: RefCell<Option<Result<TheoremWorldTextureBinding, TheoremWorldTextureError>>>,
}

pub(crate) struct Pending {
    challenge: String,
    epoch: u64,
    source: TheoremWorldSource,
}

#[derive(JSTraceable, MallocSizeOf)]
pub(crate) struct Destination {
    context: Dom<WebGL2RenderingContext>,
    texture: Dom<WebGLTexture>,
    #[no_trace]
    #[ignore_malloc_size_of = "small native grant"]
    binding: TheoremWorldTextureBinding,
    last_frame: Cell<u64>,
}

fn texture_valid(
    context: &WebGL2RenderingContext,
    texture: &WebGLTexture,
    source: TheoremWorldSource,
) -> bool {
    let base = context.base_context();
    let Some(info) = texture.image_info_at_face(0, 0) else {
        return false;
    };
    !texture.is_invalid()
        && texture.target() == Some(glow::TEXTURE_2D)
        && texture.upcast::<WebGLObject>().context_id() == base.sender().context_id()
        && info.width() == source.width
        && info.height() == source.height
        && matches!(info.internal_format(), TexFormat::RGBA8 | TexFormat::RGBA)
}

fn attest_destination(cx: &mut JSContext, args: CallArgs) -> bool {
    let global = {
        let realm = CurrentRealm::assert(cx);
        GlobalScope::from_current_realm(&realm)
    };
    let window = global.as_window();
    let valid = (|| {
        if !window.Document().is_fully_active() || args.argc_ != 5 {
            return false;
        }
        let callback = unsafe { *args.get(0) };
        if !callback.is_object()
            || callback.to_object() != window.theorem_world_gpu().callback.get()
        {
            return false;
        }
        let pending = window.theorem_world_gpu().pending.borrow();
        let Some(pending) = pending.as_ref() else {
            return false;
        };
        matches_stamp(cx, &args, 1, pending)
    })();
    unsafe {
        args.rval().set(BooleanValue(valid));
    }
    true
}

fn matches_stamp(cx: &mut JSContext, args: &CallArgs, start: usize, pending: &Pending) -> bool {
    let challenge_value = unsafe { *args.get(start as u32) };
    let epoch_value = unsafe { *args.get((start + 1) as u32) };
    let width_value = unsafe { *args.get((start + 2) as u32) };
    let height_value = unsafe { *args.get((start + 3) as u32) };
    if !challenge_value.is_string()
        || !epoch_value.is_string()
        || !width_value.is_number()
        || !height_value.is_number()
        || width_value.to_number() != f64::from(pending.source.width)
        || height_value.to_number() != f64::from(pending.source.height)
    {
        return false;
    }
    rooted!(&in(cx) let challenge_value = challenge_value);
    rooted!(&in(cx) let epoch_value = epoch_value);
    let challenge = String::safe_from_jsval(cx, challenge_value.handle(), ());
    let epoch = String::safe_from_jsval(cx, epoch_value.handle(), ());
    matches!(challenge, Ok(ConversionResult::Success(ref value)) if value == &pending.challenge)
        && matches!(epoch, Ok(ConversionResult::Success(ref value)) if value == &pending.epoch.to_string())
}

fn receive_destination(cx: &mut JSContext, args: CallArgs) -> bool {
    // A callback retained by page code becomes inert as soon as the synchronous bootstrap ends.
    let global = {
        let realm = CurrentRealm::assert(cx);
        GlobalScope::from_current_realm(&realm)
    };
    let window = global.as_window();
    if args.callee() != window.theorem_world_gpu().callback.get() {
        args.rval().set(BooleanValue(false));
        return true;
    }
    let Some(pending) = window.theorem_world_gpu().pending.borrow_mut().take() else {
        unsafe {
            args.rval().set(BooleanValue(false));
        }
        return true;
    };
    let result = (|| {
        if !window.Document().is_fully_active() {
            return Err(TheoremWorldTextureError::DocumentUnavailable);
        }
        if args.argc_ != 6 {
            return Err(TheoremWorldTextureError::InvalidBootstrap);
        }
        rooted!(&in(cx) let context_value = unsafe { *args.get(0) });
        rooted!(&in(cx) let texture_value = unsafe { *args.get(1) });
        if !context_value.is_object()
            || !texture_value.is_object()
            || !matches_stamp(cx, &args, 2, &pending)
        {
            return Err(TheoremWorldTextureError::InvalidBootstrap);
        }
        let challenge = pending.challenge.clone();
        let context = root_from_object_static::<WebGL2RenderingContext>(context_value.to_object())
            .map_err(|_| TheoremWorldTextureError::InvalidTexture)?;
        let texture = root_from_object_static::<WebGLTexture>(texture_value.to_object())
            .map_err(|_| TheoremWorldTextureError::InvalidTexture)?;
        if context.global().as_window().pipeline_id() != window.pipeline_id()
            || texture.global().as_window().pipeline_id() != window.pipeline_id()
            || !texture_valid(&context, &texture, pending.source)
        {
            return Err(TheoremWorldTextureError::InvalidTexture);
        }
        let binding = TheoremWorldTextureBinding {
            webview: window.webview_id(),
            document: window.pipeline_id(),
            presentation_epoch: pending.epoch,
            challenge,
            source: pending.source,
            context: context.base_context().sender().context_id().0,
            texture: texture.id().get(),
        };
        window
            .theorem_world_gpu()
            .textures
            .borrow_mut()
            .push(Destination {
                context: Dom::from_ref(&context),
                texture: Dom::from_ref(&texture),
                binding: binding.clone(),
                last_frame: Cell::new(0),
            });
        Ok(binding)
    })();
    unsafe {
        args.rval().set(BooleanValue(result.is_ok()));
    }
    *window.theorem_world_gpu().registration.borrow_mut() = Some(result);
    true
}

pub(crate) fn dispatch(
    window: &Window,
    request: TheoremWorldTextureRequest,
    callback: GenericCallback<Result<TheoremWorldTextureReceipt, TheoremWorldTextureError>>,
    cx: &mut JSContext,
) {
    if !matches!(&request, TheoremWorldTextureRequest::Revoke { .. })
        && !window.Document().is_fully_active()
    {
        let _ = callback.send(Err(TheoremWorldTextureError::DocumentUnavailable));
        return;
    }
    let result = match request {
        TheoremWorldTextureRequest::Register {
            origin,
            challenge,
            presentation_epoch,
            source,
        } => register(window, origin, challenge, presentation_epoch, source, cx).map(|binding| {
            TheoremWorldTextureReceipt {
                binding,
                completed_frame: None,
            }
        }),
        TheoremWorldTextureRequest::Validate { binding } => validate_destination(window, &binding)
            .map(|completed_frame| TheoremWorldTextureReceipt {
                binding,
                completed_frame,
            }),
        TheoremWorldTextureRequest::Revoke { binding } => {
            let mut destinations = window.theorem_world_gpu().textures.borrow_mut();
            let count = destinations.len();
            destinations.retain(|destination| destination.binding != binding);
            if count == destinations.len() {
                Err(TheoremWorldTextureError::StaleBinding)
            } else {
                Ok(TheoremWorldTextureReceipt {
                    binding,
                    completed_frame: None,
                })
            }
        },
        TheoremWorldTextureRequest::Import { binding, frame } => {
            import(window, &binding, &frame, callback.clone()).map(|()| {
                TheoremWorldTextureReceipt {
                    binding,
                    completed_frame: Some(frame.frame_generation),
                }
            })
        },
    };
    let _ = callback.send(result);
}

fn register(
    window: &Window,
    origin: String,
    challenge: String,
    epoch: u64,
    source: TheoremWorldSource,
    cx: &mut JSContext,
) -> Result<TheoremWorldTextureBinding, TheoremWorldTextureError> {
    // The production mount uses only canonical loopback roots; subframes and redirects cannot register.
    let url = window.get_url();
    if url.as_str() != format!("{origin}/")
        || !origin.starts_with("http://127.0.0.1:")
        || origin[17..]
            .parse::<u16>()
            .ok()
            .filter(|port| *port != 0)
            .is_none()
    {
        return Err(TheoremWorldTextureError::OriginMismatch);
    }
    if challenge.len() != 32
        || !challenge.bytes().all(|byte| byte.is_ascii_hexdigit())
        || epoch == 0
        || source.registry == 0
        || source.surface == 0
        || source.resource_generation == 0
        || source.width == 0
        || source.height == 0
        || source.width > 4096
        || source.height > 4096
    {
        return Err(TheoremWorldTextureError::InvalidBootstrap);
    }
    if window.theorem_world_gpu().textures.borrow().len() >= 64
        || window.theorem_world_gpu().pending.borrow().is_some()
        || window
            .theorem_world_gpu()
            .textures
            .borrow()
            .iter()
            .any(|d| {
                d.binding.challenge == challenge
                    || (d.binding.presentation_epoch == epoch && d.binding.source == source)
            })
    {
        return Err(TheoremWorldTextureError::StaleBinding);
    }
    rooted!(&in(cx) let global = window.reflector().get_jsobject().get());
    if !window.theorem_world_gpu().attestor_installed.get() {
        rooted!(&in(cx) let attestor = crate::native_raw_obj_fn!(cx, attest_destination, c"attestTheoremWorldTexture", 5, 0));
        rooted!(&in(cx) let value = ObjectValue(attestor.get()));
        if !unsafe {
            JS_DefineProperty(
                cx,
                global.handle(),
                c"__theoremWorldValidateDestinationCallbackV1".as_ptr(),
                value.handle(),
                (JSPROP_READONLY | JSPROP_PERMANENT) as u32,
            )
        } {
            unsafe {
                JS_ClearPendingException(cx);
            }
            return Err(TheoremWorldTextureError::InvalidBootstrap);
        }
        window.theorem_world_gpu().attestor_installed.set(true);
    }
    rooted!(&in(cx) let mut bootstrap = UndefinedValue());
    if get_property_jsval(
        cx,
        global.handle(),
        c"__theoremWorldNativeDestinationV1",
        bootstrap.handle_mut(),
    )
    .is_err()
    {
        unsafe {
            JS_ClearPendingException(cx);
        }
        return Err(TheoremWorldTextureError::InvalidBootstrap);
    }
    if !bootstrap.is_object() {
        return Err(TheoremWorldTextureError::InvalidBootstrap);
    }
    rooted!(&in(cx) let native_callback = crate::native_raw_obj_fn!(cx, receive_destination, c"registerTheoremWorldTexture", 6, 0));
    rooted!(&in(cx) let mut challenge_value = UndefinedValue());
    challenge.safe_to_jsval(cx, challenge_value.handle_mut());
    rooted_vec!(let mut arguments);
    arguments.push(ObjectValue(native_callback.get()));
    arguments.push(challenge_value.get());
    rooted!(&in(cx) let mut epoch_value = UndefinedValue());
    epoch
        .to_string()
        .safe_to_jsval(cx, epoch_value.handle_mut());
    arguments.push(epoch_value.get());
    arguments.push(js::jsval::DoubleValue(f64::from(source.width)));
    arguments.push(js::jsval::DoubleValue(f64::from(source.height)));
    window
        .theorem_world_gpu()
        .callback
        .set(native_callback.get());
    *window.theorem_world_gpu().registration.borrow_mut() = None;
    *window.theorem_world_gpu().pending.borrow_mut() = Some(Pending {
        challenge,
        epoch,
        source,
    });
    rooted!(&in(cx) let mut ignored = UndefinedValue());
    rooted!(&in(cx) let global_value = ObjectValue(global.get()));
    let called = unsafe {
        Call(
            cx,
            global_value.handle(),
            bootstrap.handle(),
            &HandleValueArray::from(&arguments),
            ignored.handle_mut(),
        )
    };
    window.theorem_world_gpu().pending.borrow_mut().take();
    window
        .theorem_world_gpu()
        .callback
        .set(std::ptr::null_mut());
    if !called {
        unsafe {
            JS_ClearPendingException(cx);
        }
        window
            .theorem_world_gpu()
            .textures
            .borrow_mut()
            .retain(|d| d.binding.presentation_epoch != epoch || d.binding.source != source);
        return Err(TheoremWorldTextureError::InvalidBootstrap);
    }
    window
        .theorem_world_gpu()
        .registration
        .borrow_mut()
        .take()
        .unwrap_or(Err(TheoremWorldTextureError::InvalidBootstrap))
}

fn validate_destination(
    window: &Window,
    binding: &TheoremWorldTextureBinding,
) -> Result<Option<u64>, TheoremWorldTextureError> {
    if binding.document != window.pipeline_id() || binding.webview != window.webview_id() {
        return Err(TheoremWorldTextureError::StaleBinding);
    }
    let destinations = window.theorem_world_gpu().textures.borrow();
    let destination = destinations
        .iter()
        .find(|destination| &destination.binding == binding)
        .ok_or(TheoremWorldTextureError::StaleBinding)?;
    if !texture_valid(&destination.context, &destination.texture, binding.source) {
        return Err(TheoremWorldTextureError::InvalidTexture);
    }
    let completed = destination.last_frame.get();
    Ok((completed != 0).then_some(completed))
}

fn import(
    window: &Window,
    binding: &TheoremWorldTextureBinding,
    frame: &TheoremWorldFrame,
    completion_guard: GenericCallback<Result<TheoremWorldTextureReceipt, TheoremWorldTextureError>>,
) -> Result<(), TheoremWorldTextureError> {
    if binding.document != window.pipeline_id() || binding.webview != window.webview_id() {
        return Err(TheoremWorldTextureError::StaleBinding);
    }
    let destinations = window.theorem_world_gpu().textures.borrow();
    let destination = destinations
        .iter()
        .find(|destination| &destination.binding == binding)
        .ok_or(TheoremWorldTextureError::StaleBinding)?;
    validate_frame(binding, frame, destination.last_frame.get())?;
    if !texture_valid(&destination.context, &destination.texture, binding.source) {
        return Err(TheoremWorldTextureError::InvalidTexture);
    }
    // Protect before queueing: even a later graphics error may follow a partial GPU write.
    // This flag is sticky for the context lifetime, including resize and resource recovery.
    destination.context.base_context().protect_native_surface();
    let (sender, receiver) =
        webgl_channel().ok_or(TheoremWorldTextureError::DocumentUnavailable)?;
    window
        .webgl_chan()
        .ok_or(TheoremWorldTextureError::DocumentUnavailable)?
        .send(WebGLMsg::TheoremWorldImport(
            binding.clone(),
            frame.clone(),
            sender,
            completion_guard,
        ))
        .map_err(|_| TheoremWorldTextureError::DocumentUnavailable)?;
    receiver
        .recv()
        .map_err(|_| TheoremWorldTextureError::DocumentUnavailable)??;
    destination.last_frame.set(frame.frame_generation);
    Ok(())
}
