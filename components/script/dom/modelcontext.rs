/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! `document.modelContext`, the WebMCP registry for one document.
//!
//! The Community Group draft of 27 October 2026 puts tools on the document.
//! This object holds them, so the registry dies when the document does.
//! Cross-origin visibility is decided by the CDP session, which receives
//! every tool together with this document's origin.

use std::cell::Cell;
use std::ffi::CString;

use devtools_traits::{ScriptToDevtoolsControlMsg, WebMcpInvokeResult, WebMcpToolRecord};
use dom_struct::dom_struct;
use js::context::JSContext;
use js::realm::CurrentRealm;
use script_bindings::callback::{ExceptionHandling, RootedCallback, TracedCallback};
use script_bindings::cell::DomRefCell;
use script_bindings::inheritance::Castable;
use script_bindings::reflector::reflect_dom_object;
use stylo_atoms::Atom;

use crate::dom::RootedPromise;
use crate::dom::bindings::codegen::Bindings::ModelContextBinding::{
    ModelContextMethods, WebMcpExecuteCallback,
};
use crate::dom::bindings::error::Error;
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::bindings::root::{Dom, DomRoot};
use crate::dom::bindings::str::DOMString;
use crate::dom::document::Document;
use crate::dom::eventtarget::EventTarget;
use crate::dom::promise::Promise;

#[derive(JSTraceable, MallocSizeOf)]
#[cfg_attr(crown, crown::unrooted_must_root_lint::must_root)]
struct RegisteredTool {
    name: String,
    description: String,
    input_schema: String,
    execute: TracedCallback<WebMcpExecuteCallback>,
}

/// <https://webmachinelearning.github.io/webmcp/>
#[dom_struct]
pub(crate) struct ModelContext {
    eventtarget: EventTarget,
    document: Dom<Document>,
    tools: DomRefCell<Vec<RegisteredTool>>,
    /// Invocation ids cancelled before the call ran.
    cancelled: DomRefCell<Vec<String>>,
    next_invocation: Cell<u64>,
}

impl ModelContext {
    fn new_inherited(document: &Document) -> Self {
        Self {
            eventtarget: EventTarget::new_inherited(),
            document: Dom::from_ref(document),
            tools: DomRefCell::new(Vec::new()),
            cancelled: DomRefCell::new(Vec::new()),
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

    /// Run a registered tool and answer the CDP `WebMCP.invokeTool` command.
    pub(crate) fn invoke(
        &self,
        cx: &mut JSContext,
        invocation: &str,
        name: &str,
        arguments: &str,
    ) -> WebMcpInvokeResult {
        if self.take_cancelled(invocation) {
            return WebMcpInvokeResult {
                ok: false,
                result: String::new(),
                error: Some("cancelled".to_owned()),
            };
        }
        let callback = {
            let tools = self.tools.borrow();
            let Some(tool) = tools.iter().find(|tool| tool.name == name) else {
                return WebMcpInvokeResult {
                    ok: false,
                    result: String::new(),
                    error: Some("no such tool".to_owned()),
                };
            };
            tool.execute.clone()
        };
        match callback.Call__(cx, DOMString::from(arguments), ExceptionHandling::Report) {
            Ok(value) => WebMcpInvokeResult {
                ok: true,
                result: String::from(value),
                error: None,
            },
            Err(_) => WebMcpInvokeResult {
                ok: false,
                result: String::new(),
                error: Some("exception".to_owned()),
            },
        }
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
            .fire_event(cx, Atom::from("toolschanged"));
    }
}

impl ModelContextMethods<crate::DomTypeHolder> for ModelContext {
    fn RegisterTool(
        &self,
        cx: &mut JSContext,
        name: DOMString,
        description: DOMString,
        input_schema: DOMString,
        execute: RootedCallback<WebMcpExecuteCallback>,
    ) {
        let name = String::from(name);
        let tool = RegisteredTool {
            name: name.clone(),
            description: String::from(description),
            input_schema: String::from(input_schema),
            execute: execute.to_traced(),
        };
        let mut tools = self.tools.borrow_mut();
        if let Some(existing) = tools.iter_mut().find(|tool| tool.name == name) {
            *existing = tool;
        } else {
            tools.push(tool);
        }
        drop(tools);
        self.changed(cx);
    }

    fn UnregisterTool(&self, cx: &mut JSContext, name: DOMString) {
        let name = String::from(name);
        self.tools.borrow_mut().retain(|tool| tool.name != name);
        self.changed(cx);
    }

    fn GetTools(&self) -> Vec<DOMString> {
        self.tools
            .borrow()
            .iter()
            .map(|tool| DOMString::from(tool.name.clone()))
            .collect()
    }

    fn ExecuteTool(
        &self,
        cx: &mut CurrentRealm,
        name: DOMString,
        arguments: DOMString,
    ) -> RootedPromise {
        let invocation = self.next_invocation.get();
        self.next_invocation.set(invocation + 1);
        let result = self.invoke(
            cx,
            &format!("page-{invocation}"),
            &String::from(name),
            &String::from(arguments),
        );
        if result.ok {
            Promise::new_resolved(cx, &self.global(), &DOMString::from(result.result))
        } else {
            let promise = Promise::new_in_realm(cx);
            let message = CString::new(result.error.unwrap_or_else(|| "tool failed".to_owned()))
                .unwrap_or_else(|_| c"tool failed".to_owned());
            promise.reject_error(cx, Error::Type(message));
            promise
        }
    }

    fn Cancel(&self, _cx: &mut JSContext, invocation: DOMString) {
        self.cancel(&String::from(invocation));
    }

    event_handler!(toolschanged, GetOntoolschanged, SetOntoolschanged);
}
