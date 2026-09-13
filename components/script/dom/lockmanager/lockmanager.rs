/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::cell::Cell;
use std::rc::Rc;

use dom_struct::dom_struct;
use js::context::JSContext;
use js::jsval::UndefinedValue;
use js::realm::CurrentRealm;
use js::rust::HandleValue;
use js::rust::wrappers2::{JS_ClearPendingException, JS_GetPendingException};
use script_bindings::cell::DomRefCell;
use script_bindings::reflector::{Reflector, reflect_dom_object_with_cx};
use servo_base::generic_channel::GenericCallback;
use servo_constellation_traits::{
    ScriptToConstellationMessage, WebLockInfo, WebLockMessage, WebLockMode, WebLockResponse,
};
use uuid::Uuid;

use crate::dom::abortsignal::AbortAlgorithm;
use crate::dom::bindings::callback::ExceptionHandling;
use crate::dom::bindings::codegen::Bindings::AbortSignalBinding::AbortSignalMethods;
use crate::dom::bindings::codegen::Bindings::LockManagerBinding::{
    LockGrantedCallback, LockInfo, LockManagerMethods, LockManagerSnapshot, LockMode, LockOptions,
};
use crate::dom::bindings::codegen::Bindings::WindowBinding::WindowMethods;
use crate::dom::bindings::error::Error;
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::refcounted::Trusted;
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::bindings::root::{Dom, DomRoot};
use crate::dom::bindings::str::DOMString;
use crate::dom::bindings::trace::HashMapTracedValues;
use crate::dom::globalscope::GlobalScope;
use crate::dom::lockmanager::lock::Lock;
use crate::dom::promise::Promise;
use crate::dom::promisenativehandler::{Callback, PromiseNativeHandler};
use crate::dom::sharedworkerglobalscope::SharedWorkerGlobalScope;
use crate::dom::window::Window;
use crate::realms::enter_auto_realm;

/// A request this client sent to the constellation that has not been
/// granted, refused, or aborted yet.
/// <https://w3c.github.io/web-locks/#lock-request>
#[derive(JSTraceable, MallocSizeOf)]
struct PendingRequest {
    /// The promise `request()` returned.
    #[conditional_malloc_size_of]
    promise: Rc<Promise>,
    /// <https://w3c.github.io/web-locks/#lock-request-callback>
    #[conditional_malloc_size_of]
    callback: Rc<LockGrantedCallback>,
    name: DOMString,
    mode: LockMode,
}

/// A lock granted to this client whose waiting promise has not settled.
/// <https://w3c.github.io/web-locks/#lock>
#[derive(JSTraceable, MallocSizeOf)]
struct HeldLock {
    /// <https://w3c.github.io/web-locks/#lock-released-promise>
    #[conditional_malloc_size_of]
    promise: Rc<Promise>,
}

/// The abort algorithm `request()` adds to its `signal`.
/// <https://w3c.github.io/web-locks/#dom-lockmanager-request>
#[derive(Clone, JSTraceable, MallocSizeOf)]
#[cfg_attr(crown, crown::unrooted_must_root_lint::must_root)]
pub(crate) struct WebLockAbortRequest {
    manager: Dom<LockManager>,
    request_id: u64,
}

impl WebLockAbortRequest {
    /// Abort the request and reject its promise with the signal's abort reason.
    pub(crate) fn run(&self, cx: &mut CurrentRealm, reason: HandleValue) {
        self.manager.abort_request(cx, self.request_id, reason);
    }
}

/// <https://w3c.github.io/web-locks/#api-lock-manager>
#[dom_struct]
pub(crate) struct LockManager {
    reflector_: Reflector,
    /// The id of this client's environment settings object, reported by
    /// `query()` as `clientId`.
    /// <https://html.spec.whatwg.org/multipage/#concept-environment-id>
    client_id: String,
    /// The next request id to allocate. Ids are unique per client.
    next_request_id: Cell<u64>,
    #[ignore_malloc_size_of = "promises and callbacks"]
    pending: DomRefCell<HashMapTracedValues<u64, PendingRequest>>,
    #[ignore_malloc_size_of = "promises"]
    held: DomRefCell<HashMapTracedValues<u64, HeldLock>>,
    /// `query()` promises awaiting a snapshot.
    #[ignore_malloc_size_of = "promises"]
    queries: DomRefCell<HashMapTracedValues<u64, Rc<Promise>>>,
    /// Handler of constellation responses, created on first use.
    #[no_trace]
    result_handler: DomRefCell<Option<GenericCallback<WebLockResponse>>>,
}

impl LockManager {
    fn new_inherited() -> LockManager {
        LockManager {
            reflector_: Reflector::new(),
            client_id: Uuid::new_v4().simple().to_string(),
            next_request_id: Cell::new(0),
            pending: DomRefCell::new(HashMapTracedValues::new()),
            held: DomRefCell::new(HashMapTracedValues::new()),
            queries: DomRefCell::new(HashMapTracedValues::new()),
            result_handler: DomRefCell::new(None),
        }
    }

    pub(crate) fn new(cx: &mut JSContext, global: &GlobalScope) -> DomRoot<LockManager> {
        let manager = reflect_dom_object_with_cx(Box::new(LockManager::new_inherited()), global, cx);
        global.register_web_lock_client(manager.client_id.clone());
        manager
    }

    fn allocate_request_id(&self) -> u64 {
        let id = self.next_request_id.get();
        self.next_request_id.set(id + 1);
        id
    }

    /// Whether this's relevant global object is a `Window` whose associated
    /// `Document` is fully active, or is not a `Window` at all.
    /// Whether the relevant document is fully active. A window whose browsing
    /// context was discarded (for example a removed iframe) keeps its activity
    /// flag until the pipeline exits, so that state is checked here as well.
    fn is_fully_active(&self) -> bool {
        self.global().downcast::<Window>().is_none_or(|window| {
            window.is_alive() &&
                window.undiscarded_window_proxy().is_some() &&
                window.Document().is_fully_active()
        })
    }

    fn send(&self, message: WebLockMessage) -> bool {
        self.global()
            .script_to_constellation_chan()
            .send(ScriptToConstellationMessage::WebLock(message))
            .is_ok()
    }

    /// Set up the callback the constellation replies through, if this hasn't been done already.
    fn get_or_setup_result_handler(&self) -> GenericCallback<WebLockResponse> {
        if let Some(handler) = self.result_handler.borrow().as_ref() {
            return handler.clone();
        }

        let manager = Trusted::new(self);
        let task_source = self
            .global()
            .task_manager()
            .dom_manipulation_task_source()
            .to_sendable();
        let handler = GenericCallback::new(move |message| {
            let manager = manager.clone();
            let response = match message {
                Ok(response) => response,
                Err(error) => {
                    return error!("Error receiving a Web Locks response: {error:?}");
                },
            };
            task_source.queue(task!(web_lock_response: move |cx| {
                let manager = manager.root();
                manager.handle_response(cx, response);
            }));
        })
        .expect("Could not create a Web Locks callback");

        *self.result_handler.borrow_mut() = Some(handler.clone());
        handler
    }

    /// <https://w3c.github.io/web-locks/#dom-lockmanager-request>
    fn request_with_options(
        &self,
        realm: &mut CurrentRealm,
        name: DOMString,
        options: &LockOptions,
        callback: Rc<LockGrantedCallback>,
    ) -> Rc<Promise> {
        let global = self.global();
        let promise = Promise::new_in_realm(realm);

        // If this's relevant global object's associated Document is not fully
        // active, return a promise rejected with an "InvalidStateError" DOMException.
        if !self.is_fully_active() {
            promise.reject_error(realm, Error::InvalidState(None));
            return promise;
        }

        // Step 1. Let environment be this's relevant settings object.
        // Step 2. Let origin be environment's origin.
        let origin = global.origin().immutable().clone();

        // Step 3. If origin is an opaque origin, then return a promise rejected
        // with a "SecurityError" DOMException.
        if !origin.is_tuple() {
            promise.reject_error(realm, Error::Security(None));
            return promise;
        }

        // Step 4. Let mode be options["mode"].
        let mode = options.mode;

        // Step 5. If name starts with U+002D HYPHEN-MINUS (-), then return a
        // promise rejected with a "NotSupportedError" DOMException.
        if name.str().starts_with('-') {
            promise.reject_error(
                realm,
                Error::NotSupported(Some("Lock names cannot start with '-'.".to_string())),
            );
            return promise;
        }

        // Step 6. If both options["steal"] and options["ifAvailable"] are true,
        // then return a promise rejected with a "NotSupportedError" DOMException.
        if options.steal && options.ifAvailable {
            promise.reject_error(
                realm,
                Error::NotSupported(Some(
                    "The 'steal' and 'ifAvailable' options cannot be used together.".to_string(),
                )),
            );
            return promise;
        }

        // Step 7. If options["steal"] is true and mode is not "exclusive", then
        // return a promise rejected with a "NotSupportedError" DOMException.
        if options.steal && mode != LockMode::Exclusive {
            promise.reject_error(
                realm,
                Error::NotSupported(Some(
                    "The 'steal' option can only be used with exclusive locks.".to_string(),
                )),
            );
            return promise;
        }

        // Step 8. If options["signal"] exists, and either of options["steal"] or
        // options["ifAvailable"] is true, then return a promise rejected with a
        // "NotSupportedError" DOMException.
        if options.signal.is_some() && (options.steal || options.ifAvailable) {
            promise.reject_error(
                realm,
                Error::NotSupported(Some(
                    "The 'signal' option cannot be used with 'steal' or 'ifAvailable'."
                        .to_string(),
                )),
            );
            return promise;
        }

        // Step 9. If options["signal"] exists and is aborted, then return a
        // promise rejected with options["signal"]'s abort reason.
        if let Some(signal) = options.signal.as_ref().filter(|signal| signal.aborted()) {
            rooted!(&in(realm) let mut reason = UndefinedValue());
            signal.Reason(reason.handle_mut());
            promise.reject(realm, reason.handle());
            return promise;
        }

        // Step 10. Let promise be a new promise.
        // Step 11. Let request be the result of running request a lock with
        // promise, the current agent, environment's id, origin, name, mode,
        // options["steal"], options["ifAvailable"], and callback.
        // Note: the queue and held set live in the constellation.
        let request_id = self.allocate_request_id();
        let result_handler = self.get_or_setup_result_handler();
        self.pending.borrow_mut().insert(
            request_id,
            PendingRequest {
                promise: promise.clone(),
                callback,
                name: name.clone(),
                mode,
            },
        );
        let sent = self.send(WebLockMessage::Request {
            origin,
            client_id: self.client_id.clone(),
            request_id,
            name: name.to_string(),
            mode: to_web_lock_mode(mode),
            if_available: options.ifAvailable,
            steal: options.steal,
            pipeline_bound: self.global().downcast::<SharedWorkerGlobalScope>().is_none(),
            result_handler,
        });
        if !sent {
            self.pending.borrow_mut().remove(&request_id);
            promise.reject_error(
                realm,
                Error::Type(c"Failed to send the lock request to the constellation".to_owned()),
            );
            return promise;
        }

        // Step 12. If options["signal"] exists, then add the following abort
        // steps to options["signal"]: abort the request request, and reject
        // promise with options["signal"]'s abort reason.
        if let Some(signal) = options.signal.as_ref() {
            signal.add(&AbortAlgorithm::WebLockRequest(WebLockAbortRequest {
                manager: Dom::from_ref(self),
                request_id,
            }));
        }

        // Step 13. Return promise.
        promise
    }

    /// <https://w3c.github.io/web-locks/#abort-the-request>
    fn abort_request(&self, cx: &mut CurrentRealm, request_id: u64, reason: HandleValue) {
        // A request that was granted or refused in the meantime is no longer
        // pending, and aborting it has no effect.
        let Some(request) = self.pending.borrow_mut().remove(&request_id) else {
            return;
        };
        // Step 1. Remove request from queue, and process the lock request queue.
        self.send(WebLockMessage::Abort {
            origin: self.global().origin().immutable().clone(),
            client_id: self.client_id.clone(),
            request_id,
        });
        // Step 2. Reject promise with signal's abort reason.
        request.promise.reject(cx, reason);
    }

    fn handle_response(&self, cx: &mut JSContext, response: WebLockResponse) {
        match response {
            WebLockResponse::Granted { request_id } => self.handle_granted(cx, request_id),
            WebLockResponse::Unavailable { request_id } => self.handle_unavailable(cx, request_id),
            WebLockResponse::Stolen { request_id } => self.handle_stolen(cx, request_id),
            WebLockResponse::Snapshot {
                request_id,
                held,
                pending,
            } => self.handle_snapshot(cx, request_id, held, pending),
        }
    }

    /// The task queued by step 1.5 of
    /// <https://w3c.github.io/web-locks/#process-the-lock-request-queue>.
    fn handle_granted(&self, cx: &mut JSContext, request_id: u64) {
        let Some(request) = self.pending.borrow_mut().remove(&request_id) else {
            // The request was aborted after the constellation granted it, so
            // the lock it holds is released at once and the callback never runs.
            self.send(WebLockMessage::Release {
                origin: self.global().origin().immutable().clone(),
                client_id: self.client_id.clone(),
                request_id,
            });
            return;
        };
        let global = self.global();

        // Step 1.5.1. Let waiting be a new promise.
        // Step 1.5.2. Let lock be a new lock with agent, clientId, mode, name,
        // waiting promise waiting, and released promise promise.
        self.held.borrow_mut().insert(
            request_id,
            HeldLock {
                promise: request.promise,
            },
        );
        let lock = Lock::new(cx, &global, request.name, request.mode);

        // Step 1.5.3. Let r be the result of invoking callback with a new Lock
        // object associated with lock as the only argument. If an exception
        // was thrown, reject waiting with the exception; otherwise resolve
        // waiting with r.
        let waiting = self.invoke_callback(cx, &global, &request.callback, Some(&lock));

        // Step 1.5.4. Upon fulfillment or rejection of waiting, release the
        // lock and settle lock's released promise the same way.
        self.react_to_waiting_promise(cx, &global, &waiting, request_id);
    }

    /// Step 6 of <https://w3c.github.io/web-locks/#request-a-lock>: the
    /// request had `ifAvailable` and was not grantable.
    fn handle_unavailable(&self, cx: &mut JSContext, request_id: u64) {
        let Some(request) = self.pending.borrow_mut().remove(&request_id) else {
            return;
        };
        let global = self.global();
        // Step 6.1.1. Let r be the result of invoking callback with null as
        // the only argument. If an exception was thrown, reject promise with
        // the exception; otherwise resolve promise with r.
        let result = self.invoke_callback(cx, &global, &request.callback, None);
        request.promise.resolve_native(cx, &result);
    }

    /// Step 5.1 of <https://w3c.github.io/web-locks/#request-a-lock>: another
    /// client stole this lock, which rejects its waiting promise with an
    /// "AbortError" DOMException and so rejects its released promise.
    fn handle_stolen(&self, cx: &mut JSContext, request_id: u64) {
        let Some(held) = self.held.borrow_mut().remove(&request_id) else {
            return;
        };
        held.promise.reject_error(
            cx,
            Error::Abort(Some("The lock was stolen by another request.".to_string())),
        );
    }

    /// Step 5.2 of <https://w3c.github.io/web-locks/#dom-lockmanager-query>.
    fn handle_snapshot(
        &self,
        cx: &mut JSContext,
        request_id: u64,
        held: Vec<WebLockInfo>,
        pending: Vec<WebLockInfo>,
    ) {
        let Some(promise) = self.queries.borrow_mut().remove(&request_id) else {
            return;
        };
        let snapshot = LockManagerSnapshot {
            held: Some(held.into_iter().map(to_lock_info).collect()),
            pending: Some(pending.into_iter().map(to_lock_info).collect()),
        };
        promise.resolve_native(cx, &snapshot);
    }

    /// Invoke the request's callback and return the promise its result was
    /// resolved with, or a promise rejected with the exception it threw.
    fn invoke_callback(
        &self,
        cx: &mut JSContext,
        global: &GlobalScope,
        callback: &LockGrantedCallback,
        lock: Option<&Lock>,
    ) -> Rc<Promise> {
        match callback.Call__(cx, lock, ExceptionHandling::Rethrow) {
            Ok(promise) => promise,
            Err(_) => {
                rooted!(&in(cx) let mut exception = UndefinedValue());
                #[expect(unsafe_code)]
                unsafe {
                    assert!(JS_GetPendingException(cx, exception.handle_mut()));
                    JS_ClearPendingException(cx);
                }
                Promise::new_rejected(cx, global, exception.get())
            },
        }
    }

    fn react_to_waiting_promise(
        &self,
        cx: &mut JSContext,
        global: &GlobalScope,
        waiting: &Rc<Promise>,
        request_id: u64,
    ) {
        rooted!(&in(cx) let mut fulfillment_handler = Some(WaitingPromiseSettledHandler {
            manager: Dom::from_ref(self),
            request_id,
            fulfilled: true,
        }));
        rooted!(&in(cx) let mut rejection_handler = Some(WaitingPromiseSettledHandler {
            manager: Dom::from_ref(self),
            request_id,
            fulfilled: false,
        }));
        let handler = PromiseNativeHandler::new(
            cx,
            global,
            fulfillment_handler.take().map(|h| Box::new(h) as Box<_>),
            rejection_handler.take().map(|h| Box::new(h) as Box<_>),
        );
        let mut realm = enter_auto_realm(cx, global);
        let cx = &mut realm.current_realm();
        waiting.append_native_handler(cx, &handler);
    }

    /// <https://w3c.github.io/web-locks/#release-the-lock>, followed by
    /// settling the lock's released promise.
    fn waiting_promise_settled(&self, cx: &mut CurrentRealm, request_id: u64, fulfilled: bool, value: HandleValue) {
        // A stolen lock was already released and its promise rejected.
        let Some(held) = self.held.borrow_mut().remove(&request_id) else {
            return;
        };
        self.send(WebLockMessage::Release {
            origin: self.global().origin().immutable().clone(),
            client_id: self.client_id.clone(),
            request_id,
        });
        if fulfilled {
            held.promise.resolve_native(cx, &value);
        } else {
            held.promise.reject_native(cx, &value);
        }
    }
}

impl LockManagerMethods<crate::DomTypeHolder> for LockManager {
    /// <https://w3c.github.io/web-locks/#dom-lockmanager-request>
    fn Request(
        &self,
        realm: &mut CurrentRealm,
        name: DOMString,
        callback: Rc<LockGrantedCallback>,
    ) -> Rc<Promise> {
        self.request_with_options(realm, name, &LockOptions::empty(), callback)
    }

    /// <https://w3c.github.io/web-locks/#dom-lockmanager-request-name-options-callback>
    fn Request_(
        &self,
        realm: &mut CurrentRealm,
        name: DOMString,
        options: &LockOptions,
        callback: Rc<LockGrantedCallback>,
    ) -> Rc<Promise> {
        self.request_with_options(realm, name, options, callback)
    }

    /// <https://w3c.github.io/web-locks/#dom-lockmanager-query>
    fn Query(&self, realm: &mut CurrentRealm) -> Rc<Promise> {
        let global = self.global();
        // Step 3. Let promise be a new promise.
        let promise = Promise::new_in_realm(realm);

        // If this's relevant global object's associated Document is not fully
        // active, return a promise rejected with an "InvalidStateError" DOMException.
        if !self.is_fully_active() {
            promise.reject_error(realm, Error::InvalidState(None));
            return promise;
        }

        // Step 1. Let origin be environment's origin.
        // Step 2. If origin is an opaque origin, then return a promise rejected
        // with a "SecurityError" DOMException.
        let origin = global.origin().immutable().clone();
        if !origin.is_tuple() {
            promise.reject_error(realm, Error::Security(None));
            return promise;
        }

        // Step 4. Run these steps in parallel: snapshot the lock state and
        // resolve promise with it.
        let request_id = self.allocate_request_id();
        let result_handler = self.get_or_setup_result_handler();
        self.queries.borrow_mut().insert(request_id, promise.clone());
        let sent = self.send(WebLockMessage::Query {
            origin,
            request_id,
            result_handler,
        });
        if !sent {
            self.queries.borrow_mut().remove(&request_id);
            promise.reject_error(
                realm,
                Error::Type(c"Failed to send the lock query to the constellation".to_owned()),
            );
        }

        // Step 5. Return promise.
        promise
    }
}

/// The fulfillment and rejection handlers of a lock's waiting promise.
#[derive(Clone, JSTraceable, MallocSizeOf)]
#[cfg_attr(crown, crown::unrooted_must_root_lint::must_root)]
struct WaitingPromiseSettledHandler {
    manager: Dom<LockManager>,
    request_id: u64,
    fulfilled: bool,
}

impl js::gc::Rootable for WaitingPromiseSettledHandler {}

impl Callback for WaitingPromiseSettledHandler {
    fn callback(&self, cx: &mut CurrentRealm, value: HandleValue) {
        self.manager
            .waiting_promise_settled(cx, self.request_id, self.fulfilled, value);
    }
}

fn to_web_lock_mode(mode: LockMode) -> WebLockMode {
    match mode {
        LockMode::Shared => WebLockMode::Shared,
        LockMode::Exclusive => WebLockMode::Exclusive,
    }
}

fn from_web_lock_mode(mode: WebLockMode) -> LockMode {
    match mode {
        WebLockMode::Shared => LockMode::Shared,
        WebLockMode::Exclusive => LockMode::Exclusive,
    }
}

fn to_lock_info(info: WebLockInfo) -> LockInfo {
    LockInfo {
        name: Some(DOMString::from(info.name)),
        mode: Some(from_web_lock_mode(info.mode)),
        clientId: Some(DOMString::from(info.client_id)),
    }
}
