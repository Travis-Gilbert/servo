/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */
use crate::dom::bindings::codegen::Bindings::SourceBufferListBinding::SourceBufferListMethods;
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::refcounted::Trusted;
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::bindings::root::{Dom, DomRoot};
use crate::dom::eventtarget::EventTarget;
use crate::dom::globalscope::GlobalScope;
use crate::dom::sourcebuffer::SourceBuffer;
use dom_struct::dom_struct;
use js::context::JSContext;
use script_bindings::cell::DomRefCell;
use script_bindings::reflector::reflect_dom_object;
use stylo_atoms::Atom;

#[dom_struct]
pub(crate) struct SourceBufferList {
    eventtarget: EventTarget,
    buffers: DomRefCell<Vec<Dom<SourceBuffer>>>,
}
impl SourceBufferList {
    pub(crate) fn new(cx: &mut JSContext, global: &GlobalScope) -> DomRoot<Self> {
        reflect_dom_object(
            cx,
            Box::new(Self {
                eventtarget: EventTarget::new_inherited(),
                buffers: DomRefCell::new(vec![]),
            }),
            global,
        )
    }
    pub(crate) fn items(&self) -> Vec<DomRoot<SourceBuffer>> {
        self.buffers
            .borrow()
            .iter()
            .map(|b| DomRoot::from_ref(&**b))
            .collect()
    }
    pub(crate) fn contains(&self, buffer: &SourceBuffer) -> bool {
        self.buffers
            .borrow()
            .iter()
            .any(|b| std::ptr::eq(&**b, buffer))
    }
    pub(crate) fn add(&self, buffer: &SourceBuffer) {
        if self.contains(buffer) {
            return;
        }
        self.buffers.borrow_mut().push(Dom::from_ref(buffer));
        self.queue_event("addsourcebuffer");
    }
    pub(crate) fn remove(&self, buffer: &SourceBuffer) {
        if !self.contains(buffer) {
            return;
        }
        self.buffers
            .borrow_mut()
            .retain(|b| !std::ptr::eq(&**b, buffer));
        self.queue_event("removesourcebuffer");
    }
    fn queue_event(&self, event: &'static str) {
        let this = Trusted::new(self);
        self.global()
            .task_manager()
            .media_element_task_source()
            .queue(task!(mse_list_event: move |cx| {
                this.root().upcast::<EventTarget>().fire_event(cx,Atom::from(event));
            }));
    }
}
impl SourceBufferListMethods<crate::DomTypeHolder> for SourceBufferList {
    fn Length(&self) -> u32 {
        self.buffers.borrow().len() as u32
    }
    fn IndexedGetter(&self, index: u32) -> Option<DomRoot<SourceBuffer>> {
        self.buffers
            .borrow()
            .get(index as usize)
            .map(|b| DomRoot::from_ref(&**b))
    }
    event_handler!(addsourcebuffer, GetOnaddsourcebuffer, SetOnaddsourcebuffer);
    event_handler!(
        removesourcebuffer,
        GetOnremovesourcebuffer,
        SetOnremovesourcebuffer
    );
}
