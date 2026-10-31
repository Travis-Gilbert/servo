/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! `document.modelContext`, the WebMCP registry for one document.
//!
//! The Community Group draft of 27 October 2026 registers a tool dictionary
//! whose `execute` callback returns a promise. JSON strings stay on the
//! inspection channel; the page and CDP boundaries convert them.

use std::cell::Cell;
use std::collections::HashMap;
use std::ffi::CString;

use devtools_traits::{ScriptToDevtoolsControlMsg, WebMcpInvokeResult, WebMcpToolRecord};
use dom_struct::dom_struct;
use js::context::JSContext;
use js::conversions::{ConversionResult, FromJSValConvertible};
use js::jsval::{ObjectValue, UndefinedValue};
use js::realm::CurrentRealm;
use js::rust::HandleValue;
use js::rust::wrappers2::{JS_ClearPendingException, JS_NewPlainObject, JS_ParseJSON};
use script_bindings::callback::{ExceptionHandling, TracedCallback};
use script_bindings::cell::DomRefCell;
use script_bindings::inheritance::Castable;
use script_bindings::reflector::reflect_dom_object;
use script_bindings::trace::RootedTraceableBox;
use servo_base::generic_channel::GenericSender;
use stylo_atoms::Atom;

use crate::dom::bindings::codegen::Bindings::ModelContextBinding::{
    ModelContextMethods, ModelContextTool, RegisteredTool, ToolExecuteCallback,
};
use crate::dom::bindings::error::Error;
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::bindings::root::{Dom, DomRoot};
use crate::dom::bindings::str::{DOMString, serialize_jsval_to_json_utf8};
use crate::dom::document::Document;
use crate::dom::eventtarget::EventTarget;
use crate::dom::promise::{Promise, RootedPromise};
use crate::dom::promisenativehandler::{Callback, PromiseNativeHandler};
use crate::realms::enter_auto_realm;

#[derive(JSTraceable, MallocSizeOf)]
#[cfg_attr(crown, crown::unrooted_must_root_lint::must_root)]
struct Registered {
    name: String,
    description: String,
    input_schema: String,
    execute: TracedCallback<ToolExecuteCallback>,
}

/// <https://webmachinelearning.github.io/webmcp/>
#[dom_struct]
pub(crate) struct ModelContext {
    eventtarget: EventTarget,
    document: Dom<Document>,
    tools: DomRefCell<Vec<Registered>>,
    /// Invocation ids cancelled before their promise settles.
    cancelled: DomRefCell<Vec<String>>,
    /// One reply per in-flight CDP invocation. The promise handler sends it.
    #[no_trace]
    pending_replies: DomRefCell<HashMap<String, GenericSender<WebMcpInvokeResult>>>,
    next_invocation: Cell<u64>,
}

impl ModelContext {
    fn new_inherited(document: &Document) -> Self {
        Self {
            eventtarget: EventTarget::new_inherited(),
            document: Dom::from_ref(document),
            tools: DomRefCell::new(Vec::new()),
            cancelled: DomRefCell::new(Vec::new()),
            pending_replies: DomRefCell::new(HashMap::new()),
            next_invocation: Cell::new(1),
        }
    }

    pub(crate) fn new(cx: &mut JSContext, document: &Document) -> DomRoot<Self> {
        reflect_dom_object(
            cx,
            Box::new(Self::new_inherited(document)),
            document.window(),
        )
    }

    /// Start a tool and send exactly one reply when its promise settles.
    pub(crate) fn invoke(
        &self,
        cx: &mut JSContext,
        invocation: &str,
        name: &str,
        arguments: &str,
        reply: GenericSender<WebMcpInvokeResult>,
    ) {
        if self.take_cancelled(invocation) {
            let _ = reply.send(cancelled_result());
            return;
        }
        self.invoke_named(cx, invocation, name, arguments, reply);
    }

    fn invoke_named(
        &self,
        cx: &mut JSContext,
        invocation: &str,
        name: &str,
        arguments: &str,
        reply: GenericSender<WebMcpInvokeResult>,
    ) {
        let Some(callback) = self.callback_named(name) else {
            let _ = reply.send(error_result("no such tool"));
            return;
        };
        self.invoke_callback(cx, invocation, arguments, reply, &callback);
    }

    fn callback_named(&self, name: &str) -> Option<TracedCallback<ToolExecuteCallback>> {
        self.tools
            .borrow()
            .iter()
            .find(|tool| tool.name == name)
            .map(|tool| tool.execute.clone())
    }

    fn invoke_callback(
        &self,
        cx: &mut JSContext,
        invocation: &str,
        arguments: &str,
        reply: GenericSender<WebMcpInvokeResult>,
        callback: &TracedCallback<ToolExecuteCallback>,
    ) {
        if let Ok(promise) = call_execute(cx, callback, arguments) {
            self.follow_promise(cx, invocation, reply, &promise);
            return;
        }
        let _ = reply.send(error_result("exception"));
    }

    pub(crate) fn cancel(&self, invocation: &str) {
        self.cancelled.borrow_mut().push(invocation.to_owned());
    }

    fn take_cancelled(&self, invocation: &str) -> bool {
        let mut cancelled = self.cancelled.borrow_mut();
        if let Some(index) = cancelled.iter().position(|id| id == invocation) {
            cancelled.remove(index);
            true
        } else {
            false
        }
    }

    fn follow_promise(
        &self,
        cx: &mut JSContext,
        invocation: &str,
        reply: GenericSender<WebMcpInvokeResult>,
        promise: &RootedPromise,
    ) {
        self.pending_replies
            .borrow_mut()
            .insert(invocation.to_owned(), reply);
        let global = self.global();
        rooted!(&in(cx) let mut fulfillment_handler = Some(InvokeSettled {
            context: Dom::from_ref(self),
            invocation: invocation.to_owned(),
            fulfilled: true,
        }));
        rooted!(&in(cx) let mut rejection_handler = Some(InvokeSettled {
            context: Dom::from_ref(self),
            invocation: invocation.to_owned(),
            fulfilled: false,
        }));
        let handler = PromiseNativeHandler::new(
            cx,
            &global,
            fulfillment_handler
                .take()
                .map(|handler| Box::new(handler) as Box<_>),
            rejection_handler
                .take()
                .map(|handler| Box::new(handler) as Box<_>),
        );
        let mut realm = enter_auto_realm(cx, &*global);
        let cx = &mut realm.current_realm();
        promise.append_native_handler(cx, &handler);
    }

    fn settle(&self, cx: &mut CurrentRealm, invocation: &str, fulfilled: bool, value: HandleValue) {
        let Some(reply) = self.pending_replies.borrow_mut().remove(invocation) else {
            return;
        };
        let _ = reply.send(self.settled_result(cx, invocation, fulfilled, value));
    }

    fn settled_result(
        &self,
        cx: &mut CurrentRealm,
        invocation: &str,
        fulfilled: bool,
        value: HandleValue,
    ) -> WebMcpInvokeResult {
        if self.take_cancelled(invocation) {
            return cancelled_result();
        }
        settled_value(cx, fulfilled, value)
    }

    fn publish(&self) {
        let global = self.global();
        let Some(channel) = global.devtools_chan() else {
            return;
        };
        let origin = self
            .document
            .origin()
            .immutable()
            .ascii_serialization()
            .into_owned();
        let pipeline = self.document.pipeline_id();
        let frame_id = pipeline.to_string();
        let tools = self
            .tools
            .borrow()
            .iter()
            .map(|tool| WebMcpToolRecord {
                name: tool.name.clone(),
                description: tool.description.clone(),
                input_schema: tool.input_schema.clone(),
                origin: origin.clone(),
                frame_id: frame_id.clone(),
            })
            .collect();
        let _ = channel.send(ScriptToDevtoolsControlMsg::WebMcpTools(pipeline, tools));
    }

    fn changed(&self, cx: &mut JSContext) {
        self.publish();
        self.upcast::<EventTarget>()
            .fire_event(cx, Atom::from("toolchange"));
    }

    fn reject_blank(
        &self,
        cx: &mut CurrentRealm,
        name: &str,
        description: &str,
    ) -> Option<RootedPromise> {
        if name.is_empty() {
            return Some(self.rejected(cx, "tool name is empty"));
        }
        reject_description(self, cx, description)
    }

    fn register_unique(
        &self,
        cx: &mut CurrentRealm,
        tool: &ModelContextTool,
        name: String,
        description: String,
    ) -> RootedPromise {
        if self
            .tools
            .borrow()
            .iter()
            .any(|existing| existing.name == name)
        {
            return self.rejected(cx, "a tool with that name is already registered");
        }
        self.store_tool(cx, tool, name, description)
    }

    fn store_tool(
        &self,
        cx: &mut CurrentRealm,
        tool: &ModelContextTool,
        name: String,
        description: String,
    ) -> RootedPromise {
        let Some(input_schema) = schema_json(cx, tool.inputSchema.get()) else {
            return self.rejected(cx, "inputSchema is not JSON");
        };
        self.tools.borrow_mut().push(Registered {
            name,
            description,
            input_schema,
            execute: tool.execute.clone(),
        });
        self.changed(cx);
        Promise::new_resolved(cx, &self.global(), &())
    }

    fn execute_callback(
        &self,
        cx: &mut CurrentRealm,
        callback: &TracedCallback<ToolExecuteCallback>,
        input: Option<*mut js::jsapi::JSObject>,
    ) -> RootedPromise {
        let Some(arguments) = tool_arguments(cx, input) else {
            return self.rejected(cx, "input is not JSON");
        };
        self.run_callback(cx, callback, &arguments)
    }

    fn run_callback(
        &self,
        cx: &mut CurrentRealm,
        callback: &TracedCallback<ToolExecuteCallback>,
        arguments: &str,
    ) -> RootedPromise {
        if let Ok(promise) = call_execute(cx, callback, arguments) {
            return promise;
        }
        self.rejected(cx, "exception")
    }

    fn rejected(&self, cx: &mut CurrentRealm, message: &str) -> RootedPromise {
        let promise = Promise::new_in_realm(cx);
        let text = CString::new(message).unwrap_or_else(|_| c"invalid tool".to_owned());
        promise.reject_error(cx, Error::Type(text));
        promise
    }
}

impl ModelContextMethods<crate::DomTypeHolder> for ModelContext {
    fn RegisterTool(&self, cx: &mut CurrentRealm, tool: &ModelContextTool) -> RootedPromise {
        let name = String::from(tool.name.clone());
        let description = String::from(tool.description.clone());
        if let Some(promise) = self.reject_blank(cx, &name, &description) {
            return promise;
        }
        self.register_unique(cx, tool, name, description)
    }

    fn UnregisterTool(&self, cx: &mut JSContext, name: DOMString) {
        let name = String::from(name);
        let mut tools = self.tools.borrow_mut();
        let before = tools.len();
        tools.retain(|tool| tool.name != name);
        let removed = tools.len() != before;
        drop(tools);
        if removed {
            self.changed(cx);
        }
    }

    fn GetTools(&self, cx: &mut CurrentRealm) -> RootedPromise {
        let stored: Vec<(String, String, String)> = self
            .tools
            .borrow()
            .iter()
            .map(|tool| {
                (
                    tool.name.clone(),
                    tool.description.clone(),
                    tool.input_schema.clone(),
                )
            })
            .collect();
        let tools = stored
            .into_iter()
            .map(|(name, description, schema)| RegisteredTool {
                name: DOMString::from(name),
                description: Some(DOMString::from(description)),
                inputSchema: Some(schema_value(cx, &schema)),
            })
            .collect::<Vec<_>>();
        Promise::new_resolved(cx, &self.global(), &tools)
    }

    fn ExecuteTool(
        &self,
        cx: &mut CurrentRealm,
        name: DOMString,
        input: Option<*mut js::jsapi::JSObject>,
    ) -> RootedPromise {
        let Some(callback) = self.callback_named(&String::from(name)) else {
            return self.rejected(cx, "no such tool");
        };
        self.execute_callback(cx, &callback, input)
    }

    event_handler!(toolchange, GetOntoolchange, SetOntoolchange);
}

/// Fulfillment and rejection of one CDP tool invocation.
#[derive(JSTraceable, MallocSizeOf)]
#[cfg_attr(crown, crown::unrooted_must_root_lint::must_root)]
struct InvokeSettled {
    context: Dom<ModelContext>,
    invocation: String,
    fulfilled: bool,
}

impl js::gc::Rootable for InvokeSettled {}

impl Callback for InvokeSettled {
    fn callback(&self, cx: &mut CurrentRealm, value: HandleValue) {
        self.context
            .settle(cx, &self.invocation, self.fulfilled, value);
    }
}

fn reject_description(
    context: &ModelContext,
    cx: &mut CurrentRealm,
    description: &str,
) -> Option<RootedPromise> {
    if description.is_empty() {
        return Some(context.rejected(cx, "tool description is empty"));
    }
    None
}

fn tool_arguments(cx: &mut JSContext, input: Option<*mut js::jsapi::JSObject>) -> Option<String> {
    if let Some(object) = input {
        return object_json(cx, object);
    }
    Some("{}".to_owned())
}

fn settled_value(cx: &mut CurrentRealm, fulfilled: bool, value: HandleValue) -> WebMcpInvokeResult {
    if fulfilled {
        return WebMcpInvokeResult {
            ok: true,
            result: fulfillment_json(cx, value),
            error: None,
        };
    }
    WebMcpInvokeResult {
        ok: false,
        result: String::new(),
        error: Some(rejection_text(cx, value)),
    }
}

fn call_execute(
    cx: &mut JSContext,
    callback: &TracedCallback<ToolExecuteCallback>,
    arguments: &str,
) -> Result<RootedPromise, ()> {
    rooted!(&in(cx) let mut parsed = UndefinedValue());
    let object = json_or_empty(cx, arguments, parsed.handle_mut());
    callback
        .Call__(cx, object, ExceptionHandling::Report)
        .map_err(|_| ())
}

#[expect(unsafe_code)]
fn json_or_empty(
    cx: &mut JSContext,
    json: &str,
    mut parsed: js::rust::MutableHandleValue,
) -> *mut js::jsapi::JSObject {
    let text: Vec<u16> = json.encode_utf16().collect();
    let ok = unsafe { JS_ParseJSON(cx, text.as_ptr(), text.len() as u32, parsed.reborrow()) };
    if json_object(ok, &parsed) {
        return parsed.get().to_object();
    }
    empty_object(cx, parsed)
}

fn json_object(ok: bool, parsed: &js::rust::MutableHandleValue) -> bool {
    ok && parsed.get().is_object()
}

#[expect(unsafe_code)]
fn empty_object(
    cx: &mut JSContext,
    mut parsed: js::rust::MutableHandleValue,
) -> *mut js::jsapi::JSObject {
    unsafe {
        JS_ClearPendingException(cx);
        let object = JS_NewPlainObject(cx);
        parsed.set(ObjectValue(object));
        object
    }
}

fn schema_json(cx: &mut JSContext, schema: *mut js::jsapi::JSObject) -> Option<String> {
    object_json(cx, schema)
}

fn object_json(cx: &mut JSContext, object: *mut js::jsapi::JSObject) -> Option<String> {
    rooted!(&in(cx) let value = ObjectValue(object));
    serialize_jsval_to_json_utf8(cx, value.handle())
        .ok()
        .map(|text| String::from(text))
}

fn schema_value(
    cx: &mut JSContext,
    schema: &str,
) -> RootedTraceableBox<js::jsapi::Heap<*mut js::jsapi::JSObject>> {
    rooted!(&in(cx) let mut parsed = UndefinedValue());
    let object = json_or_empty(cx, schema, parsed.handle_mut());
    RootedTraceableBox::from_box(js::jsapi::Heap::boxed(object))
}

fn fulfillment_json(cx: &mut JSContext, value: HandleValue) -> String {
    serialize_jsval_to_json_utf8(cx, value)
        .map(|text| String::from(text))
        .unwrap_or_else(|_| "null".to_owned())
}

fn rejection_text(cx: &mut JSContext, value: HandleValue) -> String {
    if let Ok(ConversionResult::Success(text)) = String::from_jsval(cx, value, ()) {
        return nonempty(text);
    }
    "exception".to_owned()
}

fn nonempty(text: String) -> String {
    if text.is_empty() {
        return "exception".to_owned();
    }
    text
}

fn error_result(message: &str) -> WebMcpInvokeResult {
    WebMcpInvokeResult {
        ok: false,
        result: String::new(),
        error: Some(message.to_owned()),
    }
}

fn cancelled_result() -> WebMcpInvokeResult {
    error_result("cancelled")
}
