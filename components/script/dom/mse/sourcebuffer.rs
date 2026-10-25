/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */
use crate::dom::audio::audiotracklist::AudioTrackList;
use crate::dom::bindings::codegen::Bindings::SourceBufferBinding::{
    AppendMode, SourceBufferMethods,
};
use crate::dom::bindings::codegen::UnionTypes::ArrayBufferViewOrArrayBuffer;
use crate::dom::bindings::error::{Error, ErrorResult, Fallible};
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::refcounted::Trusted;
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::bindings::root::{Dom, DomRoot};
use crate::dom::bindings::str::DOMString;
use crate::dom::eventtarget::EventTarget;
use crate::dom::mediasource::{MediaSource, map_error};
use crate::dom::timeranges::{TimeRanges, TimeRangesContainer};
use crate::dom::videotracklist::VideoTrackList;
use crate::dom::webvtt::texttracklist::TextTrackList;
use dom_struct::dom_struct;
use js::context::JSContext;
use script_bindings::num::Finite;
use script_bindings::reflector::reflect_dom_object;
use servo_media::player::mse::{Completion, SourceBuffer as Backend};
use std::cell::Cell;
use std::sync::Arc;
use stylo_atoms::Atom;

#[dom_struct]
pub(crate) struct SourceBuffer {
    eventtarget: EventTarget,
    source: Dom<MediaSource>,
    #[no_trace]
    #[ignore_malloc_size_of = "Native media buffers"]
    backend: Arc<dyn Backend>,
    updating: Cell<bool>,
    generation: Cell<u64>,
    mode: Cell<AppendMode>,
    timestamp_offset: Cell<f64>,
    window_start: Cell<f64>,
    window_end: Cell<f64>,
    audio_tracks: Dom<AudioTrackList>,
    video_tracks: Dom<VideoTrackList>,
    text_tracks: Dom<TextTrackList>,
}
impl SourceBuffer {
    pub(crate) fn new(
        cx: &mut JSContext,
        source: &MediaSource,
        backend: Arc<dyn Backend>,
    ) -> DomRoot<Self> {
        let global = source.global();
        let window = global.as_window();
        let element = source.element().expect("open MSE has a media element");
        let audio_tracks = AudioTrackList::new(cx, window, &[], None);
        let video_tracks = VideoTrackList::new(cx, window, &[], None);
        let text_tracks = TextTrackList::new(cx, &element, window, &[]);
        reflect_dom_object(
            cx,
            Box::new(Self {
                eventtarget: EventTarget::new_inherited(),
                source: Dom::from_ref(source),
                backend,
                updating: Cell::new(false),
                generation: Cell::new(0),
                mode: Cell::new(AppendMode::Segments),
                timestamp_offset: Cell::new(0.0),
                window_start: Cell::new(0.0),
                window_end: Cell::new(f64::INFINITY),
                audio_tracks: Dom::from_ref(&audio_tracks),
                video_tracks: Dom::from_ref(&video_tracks),
                text_tracks: Dom::from_ref(&text_tracks),
            }),
            &*global,
        )
    }
    pub(crate) fn backend(&self) -> &dyn Backend {
        &*self.backend
    }
    pub(crate) fn updating(&self) -> bool {
        self.updating.get()
    }
    fn require_member(&self) -> ErrorResult {
        if !self.source.has_buffer(self) {
            Err(Error::InvalidState(None))
        } else {
            Ok(())
        }
    }
    fn require_idle(&self) -> ErrorResult {
        self.require_member()?;
        if self.updating() {
            Err(Error::InvalidState(None))
        } else {
            Ok(())
        }
    }
    fn queue_event(&self, event: &'static str) {
        let this = Trusted::new(self);
        let generation = self.generation.get();
        self.global()
            .task_manager()
            .media_element_task_source()
            .queue(task!(mse_buffer_event: move |cx| {
                let this = this.root();
                if this.generation.get() == generation {
                    this.upcast::<EventTarget>().fire_event(cx,Atom::from(event));
                }
            }));
    }
    fn begin(&self) -> Fallible<Completion> {
        self.require_idle()?;
        if self.source.is_closed() {
            return Err(Error::InvalidState(None));
        }
        self.source.reopen();
        self.updating.set(true);
        self.queue_event("updatestart");
        let this = Trusted::new(self);
        let generation = self.generation.get();
        let task_source = self
            .global()
            .task_manager()
            .media_element_task_source()
            .to_sendable();
        Ok(Box::new(move |result| {
            task_source.queue(task!(mse_operation_complete: move |_cx| {
                let this = this.root();
                if this.generation.get() != generation { return; }
                this.updating.set(false);
                if result.is_ok() {
                    this.source.activate(&this);
                    this.queue_event("update");
                } else { this.queue_event("error"); }
                this.queue_event("updateend");
            }));
        }))
    }
    fn failed_start(&self, result: servo_media::player::mse::Result<()>) -> ErrorResult {
        result.map_err(|e| {
            self.updating.set(false);
            self.generation.set(self.generation.get().wrapping_add(1));
            map_error(e)
        })
    }
    pub(crate) fn cancel(&self) {
        self.generation.set(self.generation.get().wrapping_add(1));
        if self.updating.replace(false) {
            let _ = self.backend.abort();
            self.queue_event("abort");
            self.queue_event("updateend");
        }
    }
}
impl SourceBufferMethods<crate::DomTypeHolder> for SourceBuffer {
    fn Mode(&self) -> AppendMode {
        self.mode.get()
    }
    fn SetMode(&self, mode: AppendMode) -> ErrorResult {
        self.require_idle()?;
        self.source.reopen();
        self.backend
            .set_sequence(mode == AppendMode::Sequence)
            .map_err(map_error)?;
        self.mode.set(mode);
        Ok(())
    }
    fn Updating(&self) -> bool {
        self.updating()
    }
    fn GetBuffered(&self, cx: &mut JSContext) -> Fallible<DomRoot<TimeRanges>> {
        self.require_member()?;
        let mut ranges = TimeRangesContainer::default();
        for (start, end) in self.backend.buffered().map_err(map_error)? {
            let _ = ranges.add(start, end);
        }
        Ok(TimeRanges::new(cx, self.global().as_window(), ranges))
    }
    fn TimestampOffset(&self) -> Finite<f64> {
        Finite::wrap(self.timestamp_offset.get())
    }
    fn SetTimestampOffset(&self, offset: Finite<f64>) -> ErrorResult {
        self.require_idle()?;
        self.source.reopen();
        self.backend
            .set_timestamp_offset(*offset)
            .map_err(map_error)?;
        self.timestamp_offset.set(*offset);
        Ok(())
    }
    fn AudioTracks(&self) -> DomRoot<AudioTrackList> {
        DomRoot::from_ref(&*self.audio_tracks)
    }
    fn VideoTracks(&self) -> DomRoot<VideoTrackList> {
        DomRoot::from_ref(&*self.video_tracks)
    }
    fn TextTracks(&self) -> DomRoot<TextTrackList> {
        DomRoot::from_ref(&*self.text_tracks)
    }
    fn AppendWindowStart(&self) -> Finite<f64> {
        Finite::wrap(self.window_start.get())
    }
    fn SetAppendWindowStart(&self, start: Finite<f64>) -> ErrorResult {
        self.require_idle()?;
        if *start < 0.0 || *start >= self.window_end.get() {
            return Err(Error::Type(c"Invalid append window".to_owned()));
        }
        self.backend
            .set_append_window(*start, self.window_end.get())
            .map_err(map_error)?;
        self.window_start.set(*start);
        Ok(())
    }
    fn AppendWindowEnd(&self) -> f64 {
        self.window_end.get()
    }
    fn SetAppendWindowEnd(&self, end: f64) -> ErrorResult {
        self.require_idle()?;
        if end.is_nan() || end <= self.window_start.get() {
            return Err(Error::Type(c"Invalid append window".to_owned()));
        }
        self.backend
            .set_append_window(self.window_start.get(), end)
            .map_err(map_error)?;
        self.window_end.set(end);
        Ok(())
    }
    fn AppendBuffer(&self, data: ArrayBufferViewOrArrayBuffer) -> ErrorResult {
        let bytes = match data {
            ArrayBufferViewOrArrayBuffer::ArrayBufferView(view) => view.to_vec(),
            ArrayBufferViewOrArrayBuffer::ArrayBuffer(buffer) => buffer.to_vec(),
        };
        let bytes = bytes.ok_or_else(|| Error::Type(c"Detached media buffer".to_owned()))?;
        let done = self.begin()?;
        self.failed_start(self.backend.append(bytes, done))
    }
    fn Abort(&self) -> ErrorResult {
        self.require_member()?;
        if !self.source.is_open() {
            return Err(Error::InvalidState(None));
        }
        self.cancel();
        self.backend
            .set_append_window(0.0, f64::INFINITY)
            .map_err(map_error)?;
        self.window_start.set(0.0);
        self.window_end.set(f64::INFINITY);
        Ok(())
    }
    fn ChangeType(&self, mime: DOMString) -> ErrorResult {
        self.require_idle()?;
        if mime.is_empty() {
            return Err(Error::Type(c"Empty MIME type".to_owned()));
        }
        self.source.reopen();
        self.backend
            .change_type(&mime.to_string())
            .map_err(map_error)
    }
    fn Remove(&self, start: Finite<f64>, end: f64) -> ErrorResult {
        self.require_idle()?;
        let duration = self.source.backend().duration();
        if *start < 0.0 || duration.is_nan() || *start > duration || end.is_nan() || end <= *start {
            return Err(Error::Type(c"Invalid removal range".to_owned()));
        }
        let done = self.begin()?;
        self.failed_start(self.backend.remove(*start, end, done))
    }
    event_handler!(updatestart, GetOnupdatestart, SetOnupdatestart);
    event_handler!(update, GetOnupdate, SetOnupdate);
    event_handler!(updateend, GetOnupdateend, SetOnupdateend);
    event_handler!(error, GetOnerror, SetOnerror);
    event_handler!(abort, GetOnabort, SetOnabort);
}
