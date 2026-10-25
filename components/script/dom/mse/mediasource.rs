/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */
use crate::dom::bindings::codegen::Bindings::MediaSourceBinding::{
    EndOfStreamError, MediaSourceMethods, ReadyState,
};
use crate::dom::bindings::error::{Error, ErrorResult, Fallible};
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::refcounted::Trusted;
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::bindings::root::{Dom, DomRoot, MutNullableDom};
use crate::dom::bindings::str::DOMString;
use crate::dom::eventtarget::EventTarget;
use crate::dom::html::htmlmediaelement::HTMLMediaElement;
use crate::dom::sourcebuffer::SourceBuffer;
use crate::dom::sourcebufferlist::SourceBufferList;
use crate::dom::window::Window;
use dom_struct::dom_struct;
use js::context::JSContext;
use js::rust::HandleObject;
use script_bindings::num::Finite;
use script_bindings::reflector::reflect_dom_object_with_proto;
use servo_media::ServoMedia;
use servo_media::player::mse::{MediaSource as Backend, MseError};
use std::cell::Cell;
use std::sync::Arc;
use stylo_atoms::Atom;

pub(crate) fn map_error(error: MseError) -> Error {
    match error {
        MseError::InvalidState => Error::InvalidState(None),
        MseError::NotSupported => Error::NotSupported(None),
        MseError::Type => Error::Type(c"Invalid media source argument".to_owned()),
        MseError::QuotaExceeded => Error::QuotaExceeded {
            quota: None,
            requested: None,
        },
        MseError::Backend(message) => Error::Operation(Some(message)),
    }
}
#[dom_struct]
pub(crate) struct MediaSource {
    eventtarget: EventTarget,
    #[no_trace]
    #[ignore_malloc_size_of = "Native media buffers"]
    backend: Arc<dyn Backend>,
    state: Cell<ReadyState>,
    buffers: Dom<SourceBufferList>,
    active: Dom<SourceBufferList>,
    element: MutNullableDom<HTMLMediaElement>,
}
impl MediaSource {
    pub(crate) fn element(&self) -> Option<DomRoot<HTMLMediaElement>> {
        self.element.get()
    }
    pub(crate) fn backend(&self) -> &dyn Backend {
        &*self.backend
    }
    pub(crate) fn is_open(&self) -> bool {
        self.state.get() == ReadyState::Open
    }
    pub(crate) fn is_closed(&self) -> bool {
        self.state.get() == ReadyState::Closed
    }
    pub(crate) fn has_buffer(&self, buffer: &SourceBuffer) -> bool {
        self.buffers.contains(buffer)
    }
    pub(crate) fn activate(&self, buffer: &SourceBuffer) {
        if buffer
            .backend()
            .buffered()
            .is_ok_and(|ranges| !ranges.is_empty())
        {
            self.active.add(buffer);
        }
    }
    // MSE defines element buffered as the intersection of active SourceBuffers.
    // Native parsing/range calculation stays in GStreamer; this combines the DOM lists.
    pub(crate) fn buffered(&self) -> crate::dom::timeranges::TimeRangesContainer {
        use crate::dom::timeranges::TimeRangesContainer;
        let ranges: Vec<_> = self
            .active
            .items()
            .iter()
            .map(|buffer| buffer.backend().buffered().unwrap_or_default())
            .collect();
        let highest_end = ranges
            .iter()
            .filter_map(|r| r.last().map(|r| r.1))
            .fold(0.0, f64::max);
        let mut intersection = vec![(0.0_f64, highest_end)];
        if ranges.is_empty() {
            intersection.clear();
        }
        for mut ranges in ranges {
            if self.state.get() == ReadyState::Ended {
                if let Some(last) = ranges.last_mut() {
                    last.1 = highest_end;
                }
            }
            intersection = intersection
                .iter()
                .flat_map(|&(a, b)| {
                    ranges.iter().filter_map(move |&(c, d)| {
                        let (start, end) = (a.max(c), b.min(d));
                        (start < end).then_some((start, end))
                    })
                })
                .collect();
        }
        let mut result = TimeRangesContainer::default();
        for (start, end) in intersection {
            let _ = result.add(start, end);
        }
        result
    }
    pub(crate) fn reopen(&self) {
        if self.state.get() == ReadyState::Ended {
            self.state.set(ReadyState::Open);
            self.queue_event("sourceopen");
        }
    }
    pub(crate) fn attach(&self, element: &HTMLMediaElement) -> ErrorResult {
        if !self.is_closed() {
            return Err(Error::InvalidState(None));
        }
        self.element.set(Some(element));
        self.state.set(ReadyState::Open);
        self.queue_event("sourceopen");
        Ok(())
    }
    pub(crate) fn detach(&self) {
        for buffer in self.buffers.items() {
            buffer.cancel();
            self.active.remove(&buffer);
            self.buffers.remove(&buffer);
        }
        self.backend.detach();
        self.element.set(None);
        self.state.set(ReadyState::Closed);
        self.queue_event("sourceclose");
    }
    fn queue_event(&self, event: &'static str) {
        let this = Trusted::new(self);
        self.global()
            .task_manager()
            .media_element_task_source()
            .queue(task!(mse_source_event: move |cx| {
                this.root().upcast::<EventTarget>().fire_event(cx, Atom::from(event));
            }));
    }
    fn require_idle(&self) -> ErrorResult {
        if !self.is_open() || self.buffers.items().iter().any(|b| b.updating()) {
            Err(Error::InvalidState(None))
        } else {
            Ok(())
        }
    }
}
impl MediaSourceMethods<crate::DomTypeHolder> for MediaSource {
    fn Constructor(
        cx: &mut JSContext,
        window: &Window,
        proto: Option<HandleObject>,
    ) -> Fallible<DomRoot<Self>> {
        let backend = ServoMedia::get()
            .create_mse_source()
            .ok_or(Error::NotSupported(None))?;
        let global = window.as_global_scope();
        let buffers = SourceBufferList::new(cx, global);
        let active = SourceBufferList::new(cx, global);
        Ok(reflect_dom_object_with_proto(
            cx,
            Box::new(Self {
                eventtarget: EventTarget::new_inherited(),
                backend,
                state: Cell::new(ReadyState::Closed),
                buffers: Dom::from_ref(&buffers),
                active: Dom::from_ref(&active),
                element: MutNullableDom::new(None),
            }),
            global,
            proto,
        ))
    }
    fn SourceBuffers(&self) -> DomRoot<SourceBufferList> {
        DomRoot::from_ref(&*self.buffers)
    }
    fn ActiveSourceBuffers(&self) -> DomRoot<SourceBufferList> {
        DomRoot::from_ref(&*self.active)
    }
    fn ReadyState(&self) -> ReadyState {
        self.state.get()
    }
    fn Duration(&self) -> f64 {
        if self.is_closed() {
            f64::NAN
        } else {
            self.backend.duration()
        }
    }
    fn SetDuration(&self, value: f64) -> ErrorResult {
        if value.is_nan() || value < 0.0 {
            return Err(Error::Type(c"Invalid duration".to_owned()));
        }
        self.require_idle()?;
        self.backend.set_duration(value).map_err(map_error)
    }
    fn CanConstructInDedicatedWorker(_: &Window) -> bool {
        false
    }
    fn AddSourceBuffer(
        &self,
        cx: &mut JSContext,
        mime: DOMString,
    ) -> Fallible<DomRoot<SourceBuffer>> {
        if mime.is_empty() {
            return Err(Error::Type(c"Empty MIME type".to_owned()));
        }
        if !Self::IsTypeSupported(self.global().as_window(), mime.clone()) {
            return Err(Error::NotSupported(None));
        }
        if !self.is_open() {
            return Err(Error::InvalidState(None));
        }
        let backend = self
            .backend
            .add_source_buffer(&mime.to_string())
            .map_err(map_error)?;
        let buffer = SourceBuffer::new(cx, self, backend);
        self.buffers.add(&buffer);
        Ok(buffer)
    }
    fn RemoveSourceBuffer(&self, buffer: &SourceBuffer) -> ErrorResult {
        if !self.buffers.contains(buffer) {
            return Err(Error::NotFound(None));
        }
        buffer.cancel();
        self.backend
            .remove_source_buffer(buffer.backend())
            .map_err(map_error)?;
        self.active.remove(buffer);
        self.buffers.remove(buffer);
        Ok(())
    }
    fn EndOfStream(&self, error: Option<EndOfStreamError>) -> ErrorResult {
        self.require_idle()?;
        self.backend
            .end_of_stream(error.map(|e| e == EndOfStreamError::Network))
            .map_err(map_error)?;
        self.state.set(ReadyState::Ended);
        self.queue_event("sourceended");
        Ok(())
    }
    fn SetLiveSeekableRange(&self, start: Finite<f64>, end: Finite<f64>) -> ErrorResult {
        if *start < 0.0 || *end < *start {
            return Err(Error::Type(c"Invalid live range".to_owned()));
        }
        if !self.is_open() {
            return Err(Error::InvalidState(None));
        }
        self.backend
            .set_live_range(Some((*start, *end)))
            .map_err(map_error)
    }
    fn ClearLiveSeekableRange(&self) -> ErrorResult {
        if !self.is_open() {
            return Err(Error::InvalidState(None));
        }
        self.backend.set_live_range(None).map_err(map_error)
    }
    fn IsTypeSupported(_: &Window, mime: DOMString) -> bool {
        ServoMedia::get().supports_mse_type(&mime.to_string())
    }
    event_handler!(sourceopen, GetOnsourceopen, SetOnsourceopen);
    event_handler!(sourceended, GetOnsourceended, SetOnsourceended);
    event_handler!(sourceclose, GetOnsourceclose, SetOnsourceclose);
}
